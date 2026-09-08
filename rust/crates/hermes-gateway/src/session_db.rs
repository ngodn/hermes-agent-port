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

fn native_persisted_content(_role: &str, content: Option<&Value>) -> String {
    let content = content.unwrap_or(&Value::Null);
    let Value::Array(parts) = content else {
        return encode_message_content(content);
    };
    let summaries = parts
        .iter()
        .filter_map(|part| {
            let kind = part.get("type").and_then(Value::as_str)?;
            match kind {
                "text" | "input_text" | "output_text" => part.get("text").map(|text| {
                    text.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| text.to_string())
                }),
                "image" | "image_url" | "input_image" => Some("[screenshot]".to_owned()),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    if summaries.is_empty() {
        encode_message_content(content)
    } else {
        summaries.join("\n")
    }
}

/// One message in a conversation, as needed to reconstruct history for a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
    /// Exact model-wire content used on the original turn, when it differed
    /// from the clean transcript content.
    pub api_content: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompressionHistoryMessage {
    pub id: i64,
    pub message: HistoryMessage,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_name: Option<String>,
    /// Generic reasoning text stored on assistant messages.
    pub reasoning: Option<String>,
    /// Provider-facing reasoning echo text stored on assistant messages.
    pub reasoning_content: Option<String>,
    /// JSON-encoded provider reasoning blocks.
    pub reasoning_details: Option<String>,
    /// JSON-encoded Codex Responses reasoning replay items.
    pub codex_reasoning_items: Option<String>,
    /// JSON-encoded Codex Responses message replay items.
    pub codex_message_items: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompressionSnapshot {
    pub watermark: i64,
    pub messages: Vec<CompressionHistoryMessage>,
}

/// Provider route attached to one usage delta. The empty strings used for an
/// unknown provider or billing mode match Python's `session_model_usage` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRoute<'a> {
    pub model: &'a str,
    pub provider: &'a str,
    pub base_url: &'a str,
    pub billing_mode: &'a str,
}

/// Exact durable compression-guard state for one session, read straight from
/// the `sessions` row without any wall-clock filtering. The caller compares
/// `cooldown_until` against its own clock; the load never deletes an expired
/// row. Mirrors the columns behind `hermes_state.py`'s
/// `get_compression_failure_cooldown_row`, `get_compression_ineffective_count`,
/// and `get_compression_recovery_deadline`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompressionGuardState {
    /// False when no `sessions` row exists for the id (or the id was empty).
    pub session_exists: bool,
    /// Stored failure-cooldown deadline (wall-clock epoch seconds), or `None`
    /// when the column is NULL. Exact, never expiry-filtered.
    pub cooldown_until: Option<f64>,
    /// Latest stored failure diagnostic, or `None` when NULL.
    pub cooldown_error: Option<String>,
    /// Ineffective-compaction strike count, normalized nonnegative on read.
    pub ineffective_count: i64,
    /// Anti-thrash recovery deadline (wall-clock epoch seconds); `0.0` means
    /// "not armed". Normalized nonnegative on read.
    pub recovery_deadline: f64,
}

impl Default for CompressionGuardState {
    fn default() -> Self {
        Self {
            session_exists: false,
            cooldown_until: None,
            cooldown_error: None,
            ineffective_count: 0,
            recovery_deadline: 0.0,
        }
    }
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
pub const MAX_SESSION_TITLE_LENGTH: usize = 100;

#[derive(Debug)]
pub enum SetSessionTitleError {
    Rejected(String),
    Database(rusqlite::Error),
}

impl std::fmt::Display for SetSessionTitleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => formatter.write_str(message),
            Self::Database(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SetSessionTitleError {}

impl From<rusqlite::Error> for SetSessionTitleError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

/// Apply the same user-visible cleanup as Python's `SessionDB.sanitize_title`.
pub fn sanitize_session_title(title: &str) -> anyhow::Result<Option<String>> {
    let cleaned: String = title
        .chars()
        .filter(|character| {
            let value = *character as u32;
            !matches!(value, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f)
                && !matches!(
                    value,
                    0x200b..=0x200f
                        | 0x2028..=0x202e
                        | 0x2060..=0x2069
                        | 0xfeff
                        | 0xfffc
                        | 0xfff9..=0xfffb
                )
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        return Ok(None);
    }
    let length = cleaned.chars().count();
    anyhow::ensure!(
        length <= MAX_SESSION_TITLE_LENGTH,
        "Title too long ({length} chars, max {MAX_SESSION_TITLE_LENGTH})"
    );
    Ok(Some(cleaned))
}

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
    // Python sends the full active transcript and relies on compression to
    // bound it. A fixed tail silently dropped older context and also made
    // request-pressure compression impossible to trigger accurately.
    let prior = db.load_history(&sid, 0).unwrap_or_default();
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

fn compression_lineage_root_on(conn: &Connection, id: &str) -> rusqlite::Result<String> {
    if id.is_empty() {
        return Ok(String::new());
    }
    let mut current = id.to_owned();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        if !seen.insert(current.clone()) {
            break;
        }
        let row = conn
            .query_row(
                "SELECT parent_session_id, source, model_config FROM sessions WHERE id = ?",
                [&current],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((parent_id, source, model_config)) = row else {
            break;
        };
        let is_fork = source == "tool"
            || model_config
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|config| config.as_object().cloned())
                .is_some_and(|config| {
                    let parent = parent_id.as_deref();
                    ["_branched_from", "_delegate_from"].iter().any(|key| {
                        let marker = config.get(*key).and_then(Value::as_str);
                        parent.map_or(marker.is_some(), |parent| marker == Some(parent))
                    })
                });
        if is_fork {
            break;
        }
        let Some(parent) = parent_id.filter(|parent| !parent.is_empty()) else {
            break;
        };
        let parent_is_compression = conn
            .query_row(
                "SELECT end_reason = 'compression' FROM sessions WHERE id = ?",
                [&parent],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        if !parent_is_compression {
            break;
        }
        current = parent;
    }
    Ok(current)
}

fn structured_holder_process_is_dead(holder: &str) -> bool {
    let pid = holder
        .split(':')
        .find_map(|part| part.strip_prefix("pid="))
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    if pid == Some(std::process::id()) {
        return !crate::durable_turn_lease::holder_is_active(holder);
    }
    #[cfg(unix)]
    {
        pid.is_some_and(|pid| !crate::status::pid_exists(i64::from(pid)))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

enum TailPhase {
    User,
    Assistant,
    Tools(std::collections::HashSet<String>),
}

fn phase_after_assistant(tool_calls: Option<&str>) -> Option<TailPhase> {
    let Some(raw) = tool_calls.filter(|value| !value.trim().is_empty()) else {
        return Some(TailPhase::User);
    };
    let calls = serde_json::from_str::<Value>(raw).ok()?;
    let calls = calls.as_array()?;
    if calls.is_empty() {
        return Some(TailPhase::User);
    }
    let mut pending = std::collections::HashSet::with_capacity(calls.len());
    for call in calls {
        let id = call.get("id")?.as_str()?.trim();
        if id.is_empty() || !pending.insert(id.to_owned()) {
            return None;
        }
    }
    Some(TailPhase::Tools(pending))
}

/// Validate complete user turns while preserving provider tool-call groups.
/// A retained tail must begin with a user and end with a final assistant. Each
/// tool result must answer exactly one id advertised by the preceding
/// assistant, and chained assistant tool calls remain in the same user turn.
pub(crate) fn complete_turn_sequence(rows: &[(String, Option<String>, Option<String>)]) -> bool {
    matches!(partial_turn_phase(rows), Some(TailPhase::User))
}

fn prunable_turn_sequence(rows: &[(String, Option<String>, Option<String>)]) -> bool {
    match partial_turn_phase(rows) {
        Some(TailPhase::User) => true,
        Some(TailPhase::Tools(pending)) => pending.is_empty(),
        _ => false,
    }
}

fn partial_turn_phase(rows: &[(String, Option<String>, Option<String>)]) -> Option<TailPhase> {
    let mut phase = TailPhase::User;
    for (role, tool_calls, tool_call_id) in rows {
        match &mut phase {
            TailPhase::User if role == "user" => phase = TailPhase::Assistant,
            TailPhase::Assistant if role == "assistant" => {
                let next = phase_after_assistant(tool_calls.as_deref())?;
                phase = next;
            }
            TailPhase::Tools(pending) if role == "tool" => {
                let id = tool_call_id.as_deref().map(str::trim)?;
                if id.is_empty() || !pending.remove(id) {
                    return None;
                }
            }
            TailPhase::Tools(pending) if role == "assistant" && pending.is_empty() => {
                let next = phase_after_assistant(tool_calls.as_deref())?;
                phase = next;
            }
            _ => return None,
        }
    }
    Some(phase)
}

fn active_transcript_counts(conn: &Connection, session_id: &str) -> rusqlite::Result<(i64, i64)> {
    let mut query = conn.prepare(
        "SELECT tool_calls FROM messages WHERE session_id = ? AND active = 1 ORDER BY id",
    )?;
    let tool_calls = query
        .query_map([session_id], |row| row.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let message_count = i64::try_from(tool_calls.len()).unwrap_or(i64::MAX);
    let tool_call_count = tool_calls
        .iter()
        .filter_map(|raw| raw.as_deref())
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
        .filter_map(|value| value.as_array().map(Vec::len))
        .fold(0_i64, |total, count| {
            total.saturating_add(i64::try_from(count).unwrap_or(i64::MAX))
        });
    Ok((message_count, tool_call_count))
}

fn clone_messages_by_id(
    tx: &rusqlite::Transaction<'_>,
    target_session_id: &str,
    ids: &[i64],
) -> rusqlite::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let columns = {
        let mut query = tx.prepare("PRAGMA table_info(messages)")?;
        let values = query
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        values
    };
    let cloned = columns
        .into_iter()
        .filter(|column| {
            !matches!(
                column.as_str(),
                "id" | "session_id" | "active" | "compacted"
            )
        })
        .collect::<Vec<_>>();
    if cloned.is_empty() {
        return Ok(());
    }
    let identifiers = cloned
        .iter()
        .map(|column| format!("\"{}\"", column.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "INSERT INTO messages (session_id, {identifiers}, active, compacted)
         SELECT ?, {identifiers}, 1, 0 FROM messages
         WHERE id IN ({placeholders}) ORDER BY id"
    );
    let mut values = Vec::<rusqlite::types::Value>::with_capacity(ids.len() + 1);
    values.push(target_session_id.to_owned().into());
    values.extend(ids.iter().copied().map(Into::into));
    tx.execute(&sql, rusqlite::params_from_iter(values))?;
    Ok(())
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

pub struct GatewaySessionSwitch<'a> {
    pub scope: &'a str,
    pub session_key: &'a str,
    pub entry_json: &'a str,
    pub outgoing_id: &'a str,
    pub target_id: &'a str,
    pub peer: GatewayPeer<'a>,
    pub display_name: Option<&'a str>,
    pub origin_json: Option<&'a str>,
}

pub struct GatewayCompressionPublish<'a> {
    pub scope: &'a str,
    pub session_key: &'a str,
    pub entry_json: &'a str,
    pub parent_id: &'a str,
    pub child_id: &'a str,
    pub compacted_messages: &'a [HistoryMessage],
    pub prefix_end_id: Option<i64>,
    pub tail_start_id: Option<i64>,
    pub watermark: i64,
    pub turn_lease_holder: Option<&'a str>,
}

pub struct GatewayInPlaceCompressionPublish<'a> {
    pub scope: &'a str,
    pub session_key: &'a str,
    pub session_id: &'a str,
    pub compacted_messages: &'a [HistoryMessage],
    pub prefix_end_id: Option<i64>,
    pub tail_start_id: Option<i64>,
    pub watermark: i64,
    pub turn_lease_holder: Option<&'a str>,
}

pub struct GatewayToolPrunePublish<'a> {
    pub scope: &'a str,
    pub session_key: &'a str,
    pub session_id: &'a str,
    /// Exact snapshot the pure pruning pass consumed.
    pub original_messages: &'a [CompressionHistoryMessage],
    /// Same rows and order as `original_messages`, with only prunable fields changed.
    pub pruned_messages: &'a [CompressionHistoryMessage],
    pub rearm_tokens: u64,
    pub turn_lease_holder: Option<&'a str>,
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

    /// Exact selected database path. Conversation teardown retains this path
    /// so it can reload the durable transcript without guessing profile scope.
    pub fn database_path(&self) -> &std::path::Path {
        &self.db_path
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

    /// Resolve the physical segment to the root of its compression-only
    /// lineage. Explicit branches, delegates, and tool children keep their own
    /// cache scope even when their parent was compressed.
    pub fn compression_lineage_root(&self, id: &str) -> rusqlite::Result<String> {
        let conn = self.conn.lock().unwrap();
        compression_lineage_root_on(&conn, id)
    }

    pub fn try_acquire_session_turn_lease(
        &self,
        session_id: &str,
        holder: &str,
        ttl_seconds: f64,
    ) -> rusqlite::Result<bool> {
        if session_id.is_empty() || holder.is_empty() {
            return Ok(false);
        }
        let now = now_secs();
        let expires_at = now + ttl_seconds.max(0.1);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let conversation_id = compression_lineage_root_on(&tx, session_id)?;
        let current = tx
            .query_row(
                "SELECT holder, expires_at FROM session_turn_leases WHERE conversation_id = ?",
                [&conversation_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
            )
            .optional()?;
        if let Some((current_holder, current_expiry)) = current {
            if current_expiry <= now || structured_holder_process_is_dead(&current_holder) {
                tx.execute(
                    "DELETE FROM session_turn_leases WHERE conversation_id = ? AND holder = ?",
                    params![conversation_id, current_holder],
                )?;
            }
        }
        tx.execute(
            "INSERT OR IGNORE INTO session_turn_leases
             (conversation_id, holder, acquired_at, expires_at) VALUES (?, ?, ?, ?)",
            params![conversation_id, holder, now, expires_at],
        )?;
        let owner = tx
            .query_row(
                "SELECT holder FROM session_turn_leases WHERE conversation_id = ?",
                [&conversation_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        tx.commit()?;
        Ok(owner.as_deref() == Some(holder))
    }

    pub fn refresh_session_turn_lease(
        &self,
        session_id: &str,
        holder: &str,
        ttl_seconds: f64,
    ) -> rusqlite::Result<bool> {
        if session_id.is_empty() || holder.is_empty() {
            return Ok(false);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let conversation_id = compression_lineage_root_on(&tx, session_id)?;
        let changed = tx.execute(
            "UPDATE session_turn_leases SET expires_at = ?
             WHERE conversation_id = ? AND holder = ?",
            params![now_secs() + ttl_seconds.max(0.1), conversation_id, holder],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn release_session_turn_lease(
        &self,
        session_id: &str,
        holder: &str,
    ) -> rusqlite::Result<()> {
        if session_id.is_empty() || holder.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let conversation_id = compression_lineage_root_on(&tx, session_id)?;
        tx.execute(
            "DELETE FROM session_turn_leases WHERE conversation_id = ? AND holder = ?",
            params![conversation_id, holder],
        )?;
        tx.commit()
    }

    pub fn session_turn_lease_holder(&self, session_id: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let conversation_id = compression_lineage_root_on(&conn, session_id)?;
        conn.query_row(
            "SELECT holder FROM session_turn_leases
             WHERE conversation_id = ? AND expires_at >= ?",
            params![conversation_id, now_secs()],
            |row| row.get(0),
        )
        .optional()
    }

    pub fn get_compression_tip(&self, id: &str) -> rusqlite::Result<String> {
        Ok(self
            .get_compression_chain(id)?
            .pop()
            .unwrap_or_else(|| id.to_owned()))
    }

    /// Load the exact durable compression-guard state for one session in a
    /// single read. Combines the columns behind Python's
    /// `get_compression_failure_cooldown_row`, `get_compression_ineffective_count`,
    /// and `get_compression_recovery_deadline`. Values are returned verbatim
    /// (cooldown deadline not expiry-filtered); the caller owns the wall-clock
    /// comparison. Strike count and recovery deadline are normalized
    /// nonnegative exactly like the Python getters. An empty id or absent row
    /// yields `session_exists = false` with default fields.
    pub fn load_compression_guard_state(
        &self,
        session_id: &str,
    ) -> rusqlite::Result<CompressionGuardState> {
        if session_id.is_empty() {
            return Ok(CompressionGuardState::default());
        }
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT compression_failure_cooldown_until, compression_failure_error, \
                 compression_ineffective_count, compression_recovery_deadline \
                 FROM sessions WHERE id = ?",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, Option<f64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<f64>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((cooldown_until, cooldown_error, ineffective_count, recovery_deadline)) = row
        else {
            return Ok(CompressionGuardState::default());
        };
        Ok(CompressionGuardState {
            session_exists: true,
            cooldown_until,
            cooldown_error,
            // Normalize like Python's `max(0, int(value or 0))` /
            // `max(0.0, float(value or 0.0))`, so a NULL or negative stored
            // value reads as an unarmed guard.
            ineffective_count: ineffective_count.unwrap_or(0).max(0),
            recovery_deadline: recovery_deadline.unwrap_or(0.0).max(0.0),
        })
    }

    /// Persist a compression-failure cooldown, merging with any longer live
    /// deadline so a later shorter write cannot reopen the thrash window. The
    /// error column always takes the latest diagnostic. Ported from Python's
    /// `record_compression_failure_cooldown`. Returns false (no write) for an
    /// empty id, and false when no session row matched so a missing session is
    /// never reported as a successful update.
    pub fn record_compression_failure_cooldown(
        &self,
        session_id: &str,
        cooldown_until: f64,
        error: Option<&str>,
    ) -> rusqlite::Result<bool> {
        if session_id.is_empty() {
            return Ok(false);
        }
        let changed = self.conn.lock().unwrap().execute(
            "UPDATE sessions SET compression_failure_cooldown_until = CASE \
             WHEN compression_failure_cooldown_until IS NOT NULL \
              AND compression_failure_cooldown_until > ? \
             THEN compression_failure_cooldown_until ELSE ? END, \
             compression_failure_error = ? WHERE id = ?",
            params![cooldown_until, cooldown_until, error, session_id],
        )?;
        Ok(changed == 1)
    }

    /// Clear any persisted compression-failure cooldown for a session. Ported
    /// from Python's `clear_compression_failure_cooldown`. Returns false for an
    /// empty id or when no session row matched.
    pub fn clear_compression_failure_cooldown(&self, session_id: &str) -> rusqlite::Result<bool> {
        if session_id.is_empty() {
            return Ok(false);
        }
        let changed = self.conn.lock().unwrap().execute(
            "UPDATE sessions SET compression_failure_cooldown_until = NULL, \
             compression_failure_error = NULL WHERE id = ?",
            [session_id],
        )?;
        Ok(changed == 1)
    }

    /// Atomically persist the durable anti-thrash breaker: the ineffective
    /// strike count and the recovery deadline, both normalized nonnegative.
    /// Combines Python's `set_compression_ineffective_count` and
    /// `set_compression_recovery_deadline` in one write so a resumed
    /// compressor can never observe a half-updated breaker. A zero deadline is
    /// stored as NULL, matching Python's disarmed state.
    /// Returns false for an empty id or when no session row matched.
    pub fn set_compression_breaker(
        &self,
        session_id: &str,
        ineffective_count: i64,
        recovery_deadline: f64,
    ) -> rusqlite::Result<bool> {
        if session_id.is_empty() {
            return Ok(false);
        }
        let count = ineffective_count.max(0);
        // Guard against NaN: `max` on a NaN deadline would carry it through
        // and poison later comparisons.
        let deadline = if recovery_deadline.is_finite() {
            recovery_deadline.max(0.0)
        } else {
            0.0
        };
        let stored_deadline = (deadline > 0.0).then_some(deadline);
        let changed = self.conn.lock().unwrap().execute(
            "UPDATE sessions SET compression_ineffective_count = ?, \
             compression_recovery_deadline = ? WHERE id = ?",
            params![count, stored_deadline, session_id],
        )?;
        Ok(changed == 1)
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

    /// Persist the expiry flag and durable reset boundary atomically. The
    /// update is idempotent so a watcher can safely retry after an ambiguous
    /// process interruption.
    pub fn finalize_session_expiry(&self, id: &str) -> rusqlite::Result<()> {
        if id.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE sessions SET expiry_finalized = 1 WHERE id = ?",
            [id],
        )?;
        let changed = tx.execute(
            "UPDATE sessions SET ended_at = ?, end_reason = 'session_reset' WHERE id = ? AND
            (ended_at IS NULL OR end_reason IN ('agent_close','ws_orphan_reap','superseded_by_resume','startup_orphan_reap'))",
            params![now_secs(), id],
        )?;
        if changed != 0 {
            bump_conversation_generation(&tx, id, "session_reset")?;
        }
        tx.commit()
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

    /// Commit the durable half of a gateway `/resume` as one SQLite
    /// transaction. The route row, outgoing boundary, target reopen and peer
    /// capture either all become visible or all roll back.
    pub fn switch_gateway_session(
        &self,
        change: &GatewaySessionSwitch<'_>,
    ) -> rusqlite::Result<bool> {
        if change.session_key.is_empty()
            || change.outgoing_id.is_empty()
            || change.target_id.is_empty()
            || change.entry_json.is_empty()
        {
            return Ok(false);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let target_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?)",
            [change.target_id],
            |row| row.get(0),
        )?;
        if !target_exists {
            return Ok(false);
        }

        let now = now_secs();
        let changed = tx.execute(
            "UPDATE sessions SET ended_at = ?, end_reason = 'session_switch'
             WHERE id = ? AND (ended_at IS NULL OR end_reason IN
             ('agent_close','ws_orphan_reap','superseded_by_resume','startup_orphan_reap'))",
            params![now, change.outgoing_id],
        )?;
        if changed != 0 {
            bump_conversation_generation(&tx, change.outgoing_id, "session_switch")?;
        }

        tx.execute(
            "UPDATE sessions AS child SET model_config = json_set(
                COALESCE(child.model_config, '{}'), '$._reset_from', child.parent_session_id)
             WHERE child.parent_session_id = ?
               AND json_extract(COALESCE(child.model_config, '{}'), '$._reset_from') IS NULL
               AND EXISTS (SELECT 1 FROM sessions p WHERE p.id = child.parent_session_id
                   AND p.end_reason IN ('session_reset','session_switch','idle','daily','suspended','resume_pending_expired')
                   AND child.session_key IS NOT NULL AND child.session_key != ''
                   AND child.session_key = p.session_key)",
            [change.target_id],
        )?;
        tx.execute(
            "UPDATE sessions SET ended_at = NULL, end_reason = NULL WHERE id = ?",
            [change.target_id],
        )?;

        let sql = format!(
            "{COMPRESSION_PEER_CTE} UPDATE sessions SET session_key = ?, source = ?, user_id = ?,
             chat_id = ?, chat_type = ?, thread_id = ?,
             display_name = COALESCE(?, display_name),
             origin_json = COALESCE(?, origin_json)
             WHERE id IN (SELECT id FROM compression_lineage)"
        );
        tx.execute(
            &sql,
            params![
                change.target_id,
                change.session_key,
                change.peer.source,
                change.peer.user_id,
                change.peer.chat_id,
                change.peer.chat_type,
                change.peer.thread_id,
                change.display_name,
                change.origin_json,
            ],
        )?;
        tx.execute(
            "INSERT INTO gateway_routing (scope, session_key, entry_json, updated_at)
             VALUES (?, ?, ?, ?) ON CONFLICT(scope, session_key) DO UPDATE SET
             entry_json = excluded.entry_json, updated_at = excluded.updated_at",
            params![change.scope, change.session_key, change.entry_json, now],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Publish a complete compression child and its routing pointer in one
    /// immediate transaction. The source transcript is never rewritten. The
    /// parent is closed only after the child handoff and any protected or
    /// concurrently appended tail rows are durable.
    pub fn publish_gateway_compression(
        &self,
        change: &GatewayCompressionPublish<'_>,
    ) -> rusqlite::Result<bool> {
        if change.scope.is_empty()
            || change.session_key.is_empty()
            || change.entry_json.is_empty()
            || change.parent_id.is_empty()
            || change.child_id.is_empty()
            || change.parent_id == change.child_id
            || change.compacted_messages.is_empty()
            || change
                .prefix_end_id
                .zip(change.tail_start_id)
                .is_some_and(|(prefix, tail)| prefix >= tail)
        {
            return Ok(false);
        }
        let mut expect_user = true;
        for message in change.compacted_messages {
            let valid = if expect_user {
                message.role == "user"
            } else {
                message.role == "assistant"
            };
            if !valid {
                return Ok(false);
            }
            expect_user = !expect_user;
        }
        if !expect_user {
            return Ok(false);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_secs();
        if let Some(holder) = change.turn_lease_holder {
            let conversation_id = compression_lineage_root_on(&tx, change.parent_id)?;
            let owner = tx
                .query_row(
                    "SELECT holder FROM session_turn_leases
                     WHERE conversation_id = ? AND expires_at >= ?",
                    params![conversation_id, now],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if owner.as_deref() != Some(holder) {
                return Ok(false);
            }
        }
        let durable_route = tx
            .query_row(
                "SELECT entry_json FROM gateway_routing WHERE scope = ? AND session_key = ?",
                params![change.scope, change.session_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if durable_route.is_some_and(|entry| {
            serde_json::from_str::<Value>(&entry)
                .ok()
                .and_then(|value| value["session_id"].as_str().map(str::to_owned))
                .as_deref()
                != Some(change.parent_id)
        }) {
            return Ok(false);
        }

        let role_predicate = if change.tail_start_id.is_some() {
            "id >= ?2"
        } else {
            "id > ?2"
        };
        let clone_start = change.tail_start_id.unwrap_or(change.watermark);
        let tail_rows = {
            let sql = format!(
                "SELECT id, role, tool_calls, tool_call_id FROM messages WHERE session_id = ?1 AND active = 1
                 AND {role_predicate} ORDER BY id"
            );
            let mut query = tx.prepare(&sql)?;
            let roles = query
                .query_map(params![change.parent_id, clone_start], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            roles
        };
        let prefix_rows = match change.prefix_end_id {
            Some(prefix_end) => {
                let mut query = tx.prepare(
                    "SELECT id, role, tool_calls, tool_call_id FROM messages
                     WHERE session_id = ? AND active = 1 AND id <= ? ORDER BY id",
                )?;
                let rows = query
                    .query_map(params![change.parent_id, prefix_end], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
            None => Vec::new(),
        };
        let prefix_roles = prefix_rows
            .iter()
            .map(|(_, role, calls, call_id)| (role.clone(), calls.clone(), call_id.clone()))
            .collect::<Vec<_>>();
        let tail_roles = tail_rows
            .iter()
            .map(|(_, role, calls, call_id)| (role.clone(), calls.clone(), call_id.clone()))
            .collect::<Vec<_>>();
        if !complete_turn_sequence(&prefix_roles) || !complete_turn_sequence(&tail_roles) {
            // A protected or concurrent tail ending on an unanswered user turn
            // would make the next real user message violate role alternation.
            return Ok(false);
        }
        let (title, title_source) = tx
            .query_row(
                "SELECT title, title_source FROM sessions WHERE id = ?",
                [change.parent_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?
            .unwrap_or((None, None));
        if title.is_some() || title_source.is_some() {
            tx.execute(
                "UPDATE sessions SET title = NULL, title_source = NULL WHERE id = ?",
                [change.parent_id],
            )?;
        }
        let inserted = tx.execute(
            "INSERT INTO sessions (
                 id, source, user_id, session_key, chat_id, chat_type, thread_id,
                 model, model_config, system_prompt, system_prompt_hash,
                 parent_session_id, cwd, profile_name, git_repo_root, git_branch,
                 origin_json, display_name, started_at, message_count,
                 last_activity_at, tool_names, hidden, title, title_source
             )
             SELECT ?1, source, user_id, session_key, chat_id, chat_type, thread_id,
                    model, model_config, system_prompt, system_prompt_hash,
                    id, cwd, profile_name, git_repo_root, git_branch,
                    origin_json, display_name, ?2, 0, ?2, tool_names, 0, ?4, ?5
             FROM sessions WHERE id = ?3 AND ended_at IS NULL",
            params![change.child_id, now, change.parent_id, title, title_source],
        )?;
        if inserted != 1 {
            return Ok(false);
        }

        let prefix_ids = prefix_rows.iter().map(|row| row.0).collect::<Vec<_>>();
        let tail_ids = tail_rows.iter().map(|row| row.0).collect::<Vec<_>>();
        clone_messages_by_id(&tx, change.child_id, &prefix_ids)?;

        for (index, message) in change.compacted_messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages
                    (session_id, role, content, api_content, timestamp, active,
                     _compressed_summary, compacted)
                 VALUES (?1, ?2, ?3, NULL, ?4, 1, ?5, 0)",
                params![
                    change.child_id,
                    message.role,
                    message.content,
                    now,
                    i64::from(index == 0),
                ],
            )?;
        }

        clone_messages_by_id(&tx, change.child_id, &tail_ids)?;
        let (active, tool_calls) = active_transcript_counts(&tx, change.child_id)?;
        tx.execute(
            "UPDATE sessions SET message_count = ?, tool_call_count = ?, last_activity_at = ?
             WHERE id = ?",
            params![active, tool_calls, now, change.child_id],
        )?;
        tx.execute(
            "INSERT INTO gateway_routing (scope, session_key, entry_json, updated_at)
             VALUES (?, ?, ?, ?) ON CONFLICT(scope, session_key) DO UPDATE SET
             entry_json = excluded.entry_json, updated_at = excluded.updated_at",
            params![change.scope, change.session_key, change.entry_json, now],
        )?;
        let closed = tx.execute(
            "UPDATE sessions SET ended_at = ?, end_reason = 'compression'
             WHERE id = ? AND ended_at IS NULL",
            params![now, change.parent_id],
        )?;
        if closed != 1 {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    /// Atomically replace the live transcript without changing session or
    /// route identity. Original rows remain searchable as `compacted=1`;
    /// verbatim retained and concurrent tails are archived as superseded
    /// duplicates and cloned byte-for-byte after the checkpoint pair.
    pub fn publish_gateway_in_place_compression(
        &self,
        change: &GatewayInPlaceCompressionPublish<'_>,
    ) -> rusqlite::Result<bool> {
        if change.scope.is_empty()
            || change.session_key.is_empty()
            || change.session_id.is_empty()
            || change.compacted_messages.is_empty()
            || change
                .prefix_end_id
                .zip(change.tail_start_id)
                .is_some_and(|(prefix, tail)| prefix >= tail)
        {
            return Ok(false);
        }
        let compacted_roles = change
            .compacted_messages
            .iter()
            .map(|message| (message.role.clone(), None, None))
            .collect::<Vec<_>>();
        if !complete_turn_sequence(&compacted_roles) {
            return Ok(false);
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_secs();
        if let Some(holder) = change.turn_lease_holder {
            let conversation_id = compression_lineage_root_on(&tx, change.session_id)?;
            let owner = tx
                .query_row(
                    "SELECT holder FROM session_turn_leases
                     WHERE conversation_id = ? AND expires_at >= ?",
                    params![conversation_id, now],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if owner.as_deref() != Some(holder) {
                return Ok(false);
            }
        }
        let durable_route = tx
            .query_row(
                "SELECT entry_json FROM gateway_routing WHERE scope = ? AND session_key = ?",
                params![change.scope, change.session_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if durable_route.is_some_and(|entry| {
            serde_json::from_str::<Value>(&entry)
                .ok()
                .and_then(|value| value["session_id"].as_str().map(str::to_owned))
                .as_deref()
                != Some(change.session_id)
        }) {
            return Ok(false);
        }
        let live = tx
            .query_row(
                "SELECT ended_at IS NULL FROM sessions WHERE id = ?",
                [change.session_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false);
        if !live {
            return Ok(false);
        }

        let predicate = if change.tail_start_id.is_some() {
            "id >= ?2"
        } else {
            "id > ?2"
        };
        let clone_start = change.tail_start_id.unwrap_or(change.watermark);
        let tail_rows = {
            let sql = format!(
                "SELECT id, role, tool_calls, tool_call_id FROM messages
                 WHERE session_id = ?1 AND active = 1 AND {predicate} ORDER BY id"
            );
            let mut query = tx.prepare(&sql)?;
            let rows = query
                .query_map(params![change.session_id, clone_start], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let prefix_rows = match change.prefix_end_id {
            Some(prefix_end) => {
                let mut query = tx.prepare(
                    "SELECT id, role, tool_calls, tool_call_id FROM messages
                     WHERE session_id = ? AND active = 1 AND id <= ? ORDER BY id",
                )?;
                let rows = query
                    .query_map(params![change.session_id, prefix_end], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
            None => Vec::new(),
        };
        let prefix_roles = prefix_rows
            .iter()
            .map(|(_, role, calls, call_id)| (role.clone(), calls.clone(), call_id.clone()))
            .collect::<Vec<_>>();
        let tail_roles = tail_rows
            .iter()
            .map(|(_, role, calls, call_id)| (role.clone(), calls.clone(), call_id.clone()))
            .collect::<Vec<_>>();
        if !complete_turn_sequence(&prefix_roles) || !complete_turn_sequence(&tail_roles) {
            return Ok(false);
        }
        let prefix_ids = prefix_rows
            .iter()
            .map(|(id, _, _, _)| *id)
            .collect::<Vec<_>>();
        let tail_ids = tail_rows
            .iter()
            .map(|(id, _, _, _)| *id)
            .collect::<Vec<_>>();
        let clone_ids = prefix_ids
            .iter()
            .chain(&tail_ids)
            .copied()
            .collect::<Vec<_>>();

        tx.execute(
            "UPDATE messages SET active = 0, compacted = 1
             WHERE session_id = ? AND active = 1",
            [change.session_id],
        )?;
        if !clone_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", clone_ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "UPDATE messages SET compacted = 0 WHERE session_id = ?
                 AND id IN ({placeholders})"
            );
            let mut values = Vec::<rusqlite::types::Value>::with_capacity(clone_ids.len() + 1);
            values.push(change.session_id.to_owned().into());
            values.extend(clone_ids.iter().copied().map(Into::into));
            tx.execute(&sql, rusqlite::params_from_iter(values))?;
        }
        clone_messages_by_id(&tx, change.session_id, &prefix_ids)?;
        for (index, message) in change.compacted_messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages
                    (session_id, role, content, api_content, timestamp, active,
                     _compressed_summary, compacted)
                 VALUES (?1, ?2, ?3, NULL, ?4, 1, ?5, 0)",
                params![
                    change.session_id,
                    message.role,
                    message.content,
                    now,
                    i64::from(index == 0),
                ],
            )?;
        }
        clone_messages_by_id(&tx, change.session_id, &tail_ids)?;
        let (active, tool_calls) = active_transcript_counts(&tx, change.session_id)?;
        tx.execute(
            "UPDATE sessions SET message_count = ?, tool_call_count = ?, last_activity_at = ?
             WHERE id = ?",
            params![active, tool_calls, now, change.session_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Atomically publish a deterministic tool-result prune while preserving
    /// every message column the pure pass did not rewrite. The consumed
    /// snapshot is compared byte-for-byte inside the write transaction, so a
    /// stale candidate cannot replace a transcript changed by another writer.
    pub fn publish_gateway_tool_prune(
        &self,
        change: &GatewayToolPrunePublish<'_>,
    ) -> rusqlite::Result<bool> {
        if change.scope.is_empty()
            || change.session_key.is_empty()
            || change.session_id.is_empty()
            || change.original_messages.is_empty()
            || change.original_messages.len() != change.pruned_messages.len()
            || change
                .original_messages
                .iter()
                .zip(change.pruned_messages)
                .any(|(before, after)| {
                    before.id != after.id
                        || before.message.role != after.message.role
                        || before.tool_call_id != after.tool_call_id
                        || before.tool_name != after.tool_name
                        || before.reasoning != after.reasoning
                        || before.reasoning_content != after.reasoning_content
                        || before.reasoning_details != after.reasoning_details
                        || before.codex_reasoning_items != after.codex_reasoning_items
                        || before.codex_message_items != after.codex_message_items
                })
            || change.original_messages == change.pruned_messages
        {
            return Ok(false);
        }
        let roles = change
            .pruned_messages
            .iter()
            .map(|message| {
                (
                    message.message.role.clone(),
                    message.tool_calls.clone(),
                    message.tool_call_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        if !prunable_turn_sequence(&roles) {
            return Ok(false);
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_secs();
        if let Some(holder) = change.turn_lease_holder {
            let conversation_id = compression_lineage_root_on(&tx, change.session_id)?;
            let owner = tx
                .query_row(
                    "SELECT holder FROM session_turn_leases
                     WHERE conversation_id = ? AND expires_at >= ?",
                    params![conversation_id, now],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if owner.as_deref() != Some(holder) {
                return Ok(false);
            }
        }
        let durable_route = tx
            .query_row(
                "SELECT entry_json FROM gateway_routing
                 WHERE scope = ? AND session_key = ?",
                params![change.scope, change.session_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if durable_route.is_some_and(|entry| {
            serde_json::from_str::<Value>(&entry)
                .ok()
                .and_then(|value| value["session_id"].as_str().map(str::to_owned))
                .as_deref()
                != Some(change.session_id)
        }) {
            return Ok(false);
        }
        let session = tx
            .query_row(
                "SELECT ended_at IS NULL, model_config FROM sessions WHERE id = ?",
                [change.session_id],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((true, model_config)) = session else {
            return Ok(false);
        };

        let durable = {
            let mut query = tx.prepare(
                "SELECT id, role, content, api_content, tool_call_id, tool_calls, tool_name,
                        reasoning, reasoning_content, reasoning_details,
                        codex_reasoning_items, codex_message_items
                 FROM messages WHERE session_id = ? AND active = 1 ORDER BY id",
            )?;
            let rows = query
                .query_map([change.session_id], |row| {
                    Ok(CompressionHistoryMessage {
                        id: row.get(0)?,
                        message: HistoryMessage {
                            role: row.get(1)?,
                            content: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                            api_content: row.get(3)?,
                        },
                        tool_call_id: row.get(4)?,
                        tool_calls: row.get(5)?,
                        tool_name: row.get(6)?,
                        reasoning: row.get(7)?,
                        reasoning_content: row.get(8)?,
                        reasoning_details: row.get(9)?,
                        codex_reasoning_items: row.get(10)?,
                        codex_message_items: row.get(11)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if durable != change.original_messages {
            return Ok(false);
        }

        let original_ids = durable.iter().map(|message| message.id).collect::<Vec<_>>();
        tx.execute(
            "UPDATE messages SET active = 0, compacted = 1
             WHERE session_id = ? AND active = 1",
            [change.session_id],
        )?;
        clone_messages_by_id(&tx, change.session_id, &original_ids)?;
        let cloned_ids = {
            let mut query = tx.prepare(
                "SELECT id FROM messages WHERE session_id = ? AND active = 1 ORDER BY id",
            )?;
            let rows = query
                .query_map([change.session_id], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if cloned_ids.len() != change.pruned_messages.len() {
            return Ok(false);
        }
        for (id, message) in cloned_ids.iter().zip(change.pruned_messages) {
            let updated = tx.execute(
                "UPDATE messages SET content = ?, api_content = ?, tool_calls = ?
                 WHERE id = ? AND session_id = ? AND active = 1",
                params![
                    message.message.content,
                    message.message.api_content,
                    message.tool_calls,
                    id,
                    change.session_id,
                ],
            )?;
            if updated != 1 {
                return Ok(false);
            }
        }

        let mut config = model_config
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        config.insert(
            "_proactive_prune_rearm_tokens".into(),
            Value::from(change.rearm_tokens),
        );
        let config = serde_json::to_string(&config)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let (active, tool_calls) = active_transcript_counts(&tx, change.session_id)?;
        tx.execute(
            "UPDATE sessions SET message_count = ?, tool_call_count = ?, model_config = ?
             WHERE id = ?",
            params![active, tool_calls, config, change.session_id],
        )?;
        tx.commit()?;
        Ok(true)
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

    pub fn proactive_prune_rearm_tokens(&self, session_id: &str) -> rusqlite::Result<u64> {
        let raw = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT model_config FROM sessions WHERE id = ?",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| value["_proactive_prune_rearm_tokens"].as_u64())
            .unwrap_or(0))
    }

    /// Find the newest durable route still pointing at a live session. The
    /// caller passes the pair back into the prune transaction, where it is
    /// rechecked under the writer lock before any transcript row changes.
    pub fn gateway_route_for_session(
        &self,
        session_id: &str,
    ) -> rusqlite::Result<Option<(String, String)>> {
        if session_id.is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock().unwrap();
        let mut query = conn.prepare(
            "SELECT scope, session_key, entry_json FROM gateway_routing
             ORDER BY updated_at DESC",
        )?;
        let rows = query.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (scope, key, entry) = row?;
            let matches = serde_json::from_str::<Value>(&entry)
                .ok()
                .and_then(|value| value["session_id"].as_str().map(str::to_owned))
                .as_deref()
                == Some(session_id);
            if matches {
                return Ok(Some((scope, key)));
            }
        }
        Ok(None)
    }

    /// Atomically add one or more main-loop provider calls to both the legacy
    /// session totals and the per-route usage table. This mirrors Python's
    /// incremental `update_token_counts` path. A missing session is a no-op so
    /// best-effort accounting cannot recreate a conversation after teardown.
    pub fn record_main_usage(
        &self,
        session_id: &str,
        route: &UsageRoute<'_>,
        usage: &crate::provider_usage::CanonicalUsage,
    ) -> rusqlite::Result<bool> {
        self.record_provider_usage(session_id, route, "", usage, true)
    }

    /// Record an auxiliary provider call without changing the session's main
    /// totals. Compression, title generation, and other auxiliary work stay
    /// visible in `session_model_usage` under their own task key.
    pub fn record_auxiliary_usage(
        &self,
        session_id: &str,
        task: &str,
        route: &UsageRoute<'_>,
        usage: &crate::provider_usage::CanonicalUsage,
    ) -> rusqlite::Result<bool> {
        if task.is_empty() {
            return Ok(false);
        }
        self.record_provider_usage(session_id, route, task, usage, false)
    }

    fn record_provider_usage(
        &self,
        session_id: &str,
        route: &UsageRoute<'_>,
        task: &str,
        usage: &crate::provider_usage::CanonicalUsage,
        update_session_totals: bool,
    ) -> rusqlite::Result<bool> {
        if session_id.is_empty() || usage.request_count == 0 {
            return Ok(false);
        }
        fn sql_count(value: u64) -> i64 {
            i64::try_from(value).unwrap_or(i64::MAX)
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                "SELECT model, billing_provider, COALESCE(api_call_count, 0)
                 FROM sessions WHERE id = ?",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((existing_model, existing_provider, existing_calls)) = existing else {
            return Ok(false);
        };

        if update_session_totals {
            // The requested primary route is written at session creation. If
            // it fails and a fallback produces the first billable response,
            // that first response is the authoritative route.
            if existing_calls == 0
                && (!route.model.is_empty() && !route.provider.is_empty())
                && (existing_model.as_deref() != Some(route.model)
                    || existing_provider.as_deref() != Some(route.provider))
            {
                tx.execute(
                    "UPDATE sessions SET model = ?, billing_provider = ?,
                     billing_base_url = ?, billing_mode = ? WHERE id = ?",
                    params![
                        route.model,
                        route.provider,
                        route.base_url,
                        route.billing_mode,
                        session_id
                    ],
                )?;
            }
            tx.execute(
                "UPDATE sessions SET
                    input_tokens = COALESCE(input_tokens, 0) + ?,
                    output_tokens = COALESCE(output_tokens, 0) + ?,
                    cache_read_tokens = COALESCE(cache_read_tokens, 0) + ?,
                    cache_write_tokens = COALESCE(cache_write_tokens, 0) + ?,
                    reasoning_tokens = COALESCE(reasoning_tokens, 0) + ?,
                    api_call_count = COALESCE(api_call_count, 0) + ?,
                    model = COALESCE(model, NULLIF(?, '')),
                    billing_provider = COALESCE(billing_provider, NULLIF(?, '')),
                    billing_base_url = COALESCE(billing_base_url, NULLIF(?, '')),
                    billing_mode = COALESCE(billing_mode, NULLIF(?, ''))
                 WHERE id = ?",
                params![
                    sql_count(usage.input_tokens),
                    sql_count(usage.output_tokens),
                    sql_count(usage.cache_read_tokens),
                    sql_count(usage.cache_write_tokens),
                    sql_count(usage.reasoning_tokens),
                    sql_count(usage.request_count),
                    route.model,
                    route.provider,
                    route.base_url,
                    route.billing_mode,
                    session_id,
                ],
            )?;
        }

        let now = now_secs();
        tx.execute(
            "INSERT INTO session_model_usage (
                session_id, model, billing_provider, billing_base_url,
                billing_mode, task, api_call_count, input_tokens,
                output_tokens, cache_read_tokens, cache_write_tokens,
                reasoning_tokens, first_seen, last_seen
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (
                session_id, model, billing_provider, billing_base_url,
                billing_mode, task
             ) DO UPDATE SET
                api_call_count = api_call_count + excluded.api_call_count,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens,
                reasoning_tokens = reasoning_tokens + excluded.reasoning_tokens,
                last_seen = excluded.last_seen",
            params![
                session_id,
                if route.model.is_empty() {
                    "unknown"
                } else {
                    route.model
                },
                route.provider,
                route.base_url,
                route.billing_mode,
                task,
                sql_count(usage.request_count),
                sql_count(usage.input_tokens),
                sql_count(usage.output_tokens),
                sql_count(usage.cache_read_tokens),
                sql_count(usage.cache_write_tokens),
                sql_count(usage.reasoning_tokens),
                now,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn get_session_title(&self, session_id: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT title FROM sessions WHERE id = ?",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
    }

    /// Set a manual title with Python-compatible uniqueness and provenance.
    /// The lookup, compression-ancestor transfer and compare-and-swap update
    /// share one immediate transaction so a concurrent title writer cannot be
    /// silently overwritten.
    pub fn set_user_session_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<bool, SetSessionTitleError> {
        let sanitized = sanitize_session_title(title)
            .map_err(|error| SetSessionTitleError::Rejected(error.to_string()))?;
        let title = sanitized.as_deref();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = tx
            .query_row(
                "SELECT title, title_source, hidden FROM sessions WHERE id = ?",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((current_title, current_source, hidden)) = current else {
            return Ok(false);
        };
        if hidden
            && current_title.as_deref() == Some(crate::bot_mode::BOT_CHAT_TITLE)
            && title != Some(crate::bot_mode::BOT_CHAT_TITLE)
        {
            return Err(SetSessionTitleError::Rejected(
                "This is the bot's canonical Bot Chat - its name is its identity, and renaming it would orphan the conversation. To start fresh, create a new bot instead."
                    .into(),
            ));
        }
        if let Some(title) = title {
            let conflict = tx
                .query_row(
                    "SELECT id FROM sessions WHERE title = ? AND id != ?",
                    params![title, session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(conflict_id) = conflict {
                let is_ancestor = tx
                    .query_row(
                        "WITH RECURSIVE ancestors(id) AS (
                            SELECT ?1
                            UNION
                            SELECT child.parent_session_id
                            FROM ancestors a
                            JOIN sessions child ON child.id = a.id
                            JOIN sessions parent ON parent.id = child.parent_session_id
                            WHERE parent.end_reason = 'compression'
                        )
                        SELECT 1 FROM ancestors WHERE id = ?2 AND id != ?1 LIMIT 1",
                        params![session_id, conflict_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if is_ancestor {
                    tx.execute(
                        "UPDATE sessions SET title = NULL WHERE id = ?",
                        [&conflict_id],
                    )?;
                } else {
                    return Err(SetSessionTitleError::Rejected(format!(
                        "Title '{title}' is already in use by session {conflict_id}"
                    )));
                }
            }
        }
        let changed = tx.execute(
            "UPDATE sessions SET title = ?, title_source = ?
             WHERE id = ? AND title IS ? AND title_source IS ?",
            params![
                title,
                title.map(|_| "user"),
                session_id,
                current_title,
                current_source
            ],
        )?;
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Direct ID wins over title. A base title follows its newest numbered
    /// continuation, matching the Python gateway's resume resolver.
    pub fn resolve_session_target(&self, target: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let direct = conn
            .query_row("SELECT id FROM sessions WHERE id = ?", [target], |row| {
                row.get(0)
            })
            .optional()?;
        if direct.is_some() {
            return Ok(direct);
        }
        let escaped = target
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let numbered = conn
            .query_row(
                "SELECT id FROM sessions WHERE title LIKE ? ESCAPE '\\'
                 ORDER BY started_at DESC LIMIT 1",
                [format!("{escaped} #%")],
                |row| row.get(0),
            )
            .optional()?;
        if numbered.is_some() {
            return Ok(numbered);
        }
        conn.query_row("SELECT id FROM sessions WHERE title = ?", [target], |row| {
            row.get(0)
        })
        .optional()
    }

    pub fn list_resume_sessions(
        &self,
        source: Option<&str>,
        session_key: Option<&str>,
        include_unnamed: bool,
        limit: usize,
    ) -> rusqlite::Result<Vec<Value>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT s.id, s.title, s.source, s.user_id, s.session_key, s.chat_id,
                    s.chat_type, s.thread_id, s.started_at,
                    COALESCE(s.last_activity_at, s.started_at) AS last_active,
                    COALESCE((SELECT m.content FROM messages m
                              WHERE m.session_id = s.id AND m.active = 1
                                AND m.role = 'user' AND m.content IS NOT NULL
                              ORDER BY m.timestamp ASC, m.id ASC LIMIT 1), '') AS preview
             FROM sessions s
             WHERE (?1 IS NULL OR s.source = ?1) AND (?2 IS NULL OR s.session_key = ?2)
               AND (?3 OR (s.title IS NOT NULL AND TRIM(s.title) != ''))
               AND s.source != 'tool'
               AND COALESCE(s.hidden, 0) = 0
             ORDER BY s.started_at DESC LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![source, session_key, include_unnamed, limit as i64],
            |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "title": row.get::<_, Option<String>>(1)?,
                    "source": row.get::<_, String>(2)?,
                    "user_id": row.get::<_, Option<String>>(3)?,
                    "session_key": row.get::<_, Option<String>>(4)?,
                    "chat_id": row.get::<_, Option<String>>(5)?,
                    "chat_type": row.get::<_, Option<String>>(6)?,
                    "thread_id": row.get::<_, Option<String>>(7)?,
                    "started_at": row.get::<_, f64>(8)?,
                    "last_active": row.get::<_, f64>(9)?,
                    "preview": row.get::<_, String>(10)?,
                }))
            },
        )?;
        rows.collect()
    }

    pub fn user_message_count(&self, session_id: &str) -> rusqlite::Result<usize> {
        self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ? AND active = 1 AND role = 'user'",
            [session_id],
            |row| row.get(0),
        )
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
            ("input_tokens", "INTEGER DEFAULT 0"),
            ("output_tokens", "INTEGER DEFAULT 0"),
            ("cache_read_tokens", "INTEGER DEFAULT 0"),
            ("cache_write_tokens", "INTEGER DEFAULT 0"),
            ("reasoning_tokens", "INTEGER DEFAULT 0"),
            ("billing_provider", "TEXT"),
            ("billing_base_url", "TEXT"),
            ("billing_mode", "TEXT"),
            ("api_call_count", "INTEGER DEFAULT 0"),
            ("cwd", "TEXT"),
            ("git_repo_root", "TEXT"),
            ("git_branch", "TEXT"),
            ("tool_names", "TEXT"),
            ("title", "TEXT"),
            ("title_source", "TEXT"),
            ("hidden", "INTEGER NOT NULL DEFAULT 0"),
            ("tool_call_count", "INTEGER DEFAULT 0"),
            ("compression_failure_cooldown_until", "REAL"),
            ("compression_failure_error", "TEXT"),
            (
                "compression_ineffective_count",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("compression_recovery_deadline", "REAL"),
        ] {
            if !columns.iter().any(|column| column == name) {
                // Names and declarations are static schema constants.
                tx.execute(
                    &format!("ALTER TABLE sessions ADD COLUMN {name} {declaration}"),
                    [],
                )?;
            }
        }
        tx.execute(
            "CREATE TABLE IF NOT EXISTS session_model_usage (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                billing_provider TEXT NOT NULL DEFAULT '',
                billing_base_url TEXT NOT NULL DEFAULT '',
                billing_mode TEXT NOT NULL DEFAULT '',
                task TEXT NOT NULL DEFAULT '',
                api_call_count INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                estimated_cost_usd REAL NOT NULL DEFAULT 0,
                actual_cost_usd REAL NOT NULL DEFAULT 0,
                cost_status TEXT,
                cost_source TEXT,
                first_seen REAL,
                last_seen REAL,
                PRIMARY KEY (
                    session_id, model, billing_provider, billing_base_url,
                    billing_mode, task
                )
            )",
            [],
        )?;
        tx.execute(
            "CREATE INDEX IF NOT EXISTS idx_session_model_usage_session
             ON session_model_usage(session_id)",
            [],
        )?;
        tx.execute(
            "CREATE INDEX IF NOT EXISTS idx_session_model_usage_model
             ON session_model_usage(model)",
            [],
        )?;
        tx.commit()
    }

    /// Repair Python stores whose `task` column was added after creation but
    /// never joined the primary key. Without this unconditional check, every
    /// task-aware upsert fails because its six-column conflict target does not
    /// match the legacy five-column key.
    fn heal_session_model_usage_pk(conn: &mut Connection) -> rusqlite::Result<()> {
        let columns = {
            let mut query = conn.prepare("PRAGMA table_info(session_model_usage)")?;
            let rows = query
                .query_map([], |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if columns.is_empty() {
            return Ok(());
        }
        let mut primary = columns
            .iter()
            .filter(|(_, position)| *position > 0)
            .collect::<Vec<_>>();
        primary.sort_by_key(|(_, position)| *position);
        let primary = primary
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        let expected = [
            "session_id",
            "model",
            "billing_provider",
            "billing_base_url",
            "billing_mode",
            "task",
        ];
        if primary == expected {
            return Ok(());
        }

        let has_task = columns.iter().any(|(name, _)| name == "task");
        let foreign_keys =
            conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))?;
        if foreign_keys {
            conn.pragma_update(None, "foreign_keys", false)?;
        }
        let result = (|| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute(
                "ALTER TABLE session_model_usage RENAME TO session_model_usage_legacy_pk",
                [],
            )?;
            tx.execute_batch(
                "CREATE TABLE session_model_usage (
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    model TEXT NOT NULL,
                    billing_provider TEXT NOT NULL DEFAULT '',
                    billing_base_url TEXT NOT NULL DEFAULT '',
                    billing_mode TEXT NOT NULL DEFAULT '',
                    task TEXT NOT NULL DEFAULT '',
                    api_call_count INTEGER NOT NULL DEFAULT 0,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    estimated_cost_usd REAL NOT NULL DEFAULT 0,
                    actual_cost_usd REAL NOT NULL DEFAULT 0,
                    cost_status TEXT,
                    cost_source TEXT,
                    first_seen REAL,
                    last_seen REAL,
                    PRIMARY KEY (
                        session_id, model, billing_provider, billing_base_url,
                        billing_mode, task
                    )
                );",
            )?;
            let task = if has_task { "COALESCE(task, '')" } else { "''" };
            tx.execute_batch(&format!(
                "INSERT OR IGNORE INTO session_model_usage (
                    session_id, model, billing_provider, billing_base_url,
                    billing_mode, task, api_call_count, input_tokens,
                    output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, estimated_cost_usd, actual_cost_usd,
                    cost_status, cost_source, first_seen, last_seen
                 )
                 SELECT session_id, model, COALESCE(billing_provider, ''),
                    COALESCE(billing_base_url, ''), COALESCE(billing_mode, ''),
                    {task}, api_call_count, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, reasoning_tokens,
                    estimated_cost_usd, actual_cost_usd, cost_status,
                    cost_source, first_seen, last_seen
                 FROM session_model_usage_legacy_pk;
                 DROP TABLE session_model_usage_legacy_pk;
                 CREATE INDEX IF NOT EXISTS idx_session_model_usage_session
                    ON session_model_usage(session_id);
                 CREATE INDEX IF NOT EXISTS idx_session_model_usage_model
                    ON session_model_usage(model);"
            ))?;
            tx.commit()
        })();
        if foreign_keys {
            let restore = conn.pragma_update(None, "foreign_keys", true);
            if result.is_ok() {
                restore?;
            }
        }
        result
    }

    /// Add native replay and compaction markers to older stores. The migration
    /// is one short schema transaction and performs no external work while
    /// SQLite holds its write lock.
    fn ensure_message_schema(conn: &mut Connection) -> rusqlite::Result<()> {
        let columns: Vec<String> = {
            let mut query = conn.prepare("PRAGMA table_info(messages)")?;
            let rows = query
                .query_map([], |row| row.get(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if [
            "api_content",
            "_compressed_summary",
            "compacted",
            "effect_disposition",
            "finish_reason",
            "reasoning",
            "reasoning_content",
            "reasoning_details",
            "codex_reasoning_items",
            "codex_message_items",
        ]
        .iter()
        .all(|required| columns.iter().any(|column| column == required))
        {
            return Ok(());
        }

        // Recheck after acquiring the writer because another opener may have
        // completed the same migration between the read and this transaction.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let columns: Vec<String> = {
            let mut query = tx.prepare("PRAGMA table_info(messages)")?;
            let rows = query
                .query_map([], |row| row.get(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if !columns.iter().any(|column| column == "api_content") {
            tx.execute("ALTER TABLE messages ADD COLUMN api_content TEXT", [])?;
        }
        if !columns.iter().any(|column| column == "_compressed_summary") {
            tx.execute(
                "ALTER TABLE messages ADD COLUMN _compressed_summary INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !columns.iter().any(|column| column == "compacted") {
            tx.execute(
                "ALTER TABLE messages ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        for column in [
            "effect_disposition",
            "finish_reason",
            "reasoning",
            "reasoning_content",
            "reasoning_details",
            "codex_reasoning_items",
            "codex_message_items",
        ] {
            if !columns.iter().any(|existing| existing == column) {
                tx.execute(
                    &format!("ALTER TABLE messages ADD COLUMN {column} TEXT"),
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

    pub fn load_gateway_routing_entry(
        &self,
        scope: &str,
        session_key: &str,
    ) -> rusqlite::Result<Option<String>> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT entry_json FROM gateway_routing
                 WHERE scope = ? AND session_key = ?",
                params![scope, session_key],
                |row| row.get(0),
            )
            .optional()
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
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
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
            "CREATE TABLE IF NOT EXISTS session_turn_leases (
                conversation_id TEXT PRIMARY KEY,
                holder TEXT NOT NULL,
                acquired_at REAL NOT NULL,
                expires_at REAL NOT NULL
            )",
            [],
        )?;
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
                tool_call_count INTEGER DEFAULT 0,
                last_activity_at REAL,
                tool_names TEXT,
                compression_failure_cooldown_until REAL,
                compression_failure_error TEXT,
                compression_ineffective_count INTEGER NOT NULL DEFAULT 0,
                compression_recovery_deadline REAL
            )",
            [],
        )?;
        Self::ensure_recovery_schema(conn)?;
        Self::heal_session_model_usage_pk(conn)?;
        Self::ensure_title_index(conn)?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sessions_source_key_started
             ON sessions(source, session_key, started_at DESC)",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                api_content TEXT,
                tool_call_id TEXT,
                tool_calls TEXT,
                tool_name TEXT,
                effect_disposition TEXT,
                finish_reason TEXT,
                reasoning TEXT,
                reasoning_content TEXT,
                reasoning_details TEXT,
                codex_reasoning_items TEXT,
                codex_message_items TEXT,
                display_kind TEXT,
                display_metadata TEXT,
                timestamp REAL NOT NULL,
                active INTEGER NOT NULL DEFAULT 1
            )",
            [],
        )?;
        Self::ensure_message_schema(conn)?;
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

    fn ensure_title_index(conn: &mut Connection) -> rusqlite::Result<()> {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let unique = "CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_title_unique
                      ON sessions(title) WHERE title IS NOT NULL";
        if let Err(error) = tx.execute(unique, []) {
            if !matches!(
                error,
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::ConstraintViolation,
                        ..
                    },
                    _
                )
            ) {
                return Err(error);
            }
            tx.execute(
                "UPDATE sessions AS older SET title = NULL
                 WHERE title IS NOT NULL AND EXISTS (
                    SELECT 1 FROM sessions AS newer
                    WHERE newer.title = older.title AND newer.rowid > older.rowid
                 )",
                [],
            )?;
            tx.execute(unique, [])?;
        }
        tx.execute("DROP INDEX IF EXISTS idx_sessions_title", [])?;
        tx.commit()
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

    /// Persist the exact API-bound copy for the newest matching user row.
    /// Matching the clean stored value prevents a delayed preparation from
    /// attaching one turn's recalled context to a newer message.
    pub fn set_latest_user_api_content(
        &self,
        session_id: &str,
        clean_content: &Value,
        api_content: &str,
    ) -> rusqlite::Result<bool> {
        let clean_content = encode_message_content(clean_content);
        let changed = self.conn.lock().unwrap().execute(
            "UPDATE messages SET api_content = ?1
             WHERE id = (
                 SELECT id FROM messages
                 WHERE session_id = ?2 AND role = 'user' AND active = 1
                 ORDER BY id DESC LIMIT 1
             ) AND content = ?3",
            params![api_content, session_id, clean_content],
        )?;
        Ok(changed == 1)
    }

    /// Count persisted user turns, including the current row after begin_turn.
    pub fn user_turn_count(&self, session_id: &str) -> rusqlite::Result<usize> {
        self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM messages
             WHERE session_id = ? AND role = 'user' AND active = 1",
            [session_id],
            |row| row.get(0),
        )
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
        let num_tool_calls = opts
            .tool_calls
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| value.as_array().map(Vec::len))
            .map_or(0_i64, |count| i64::try_from(count).unwrap_or(i64::MAX));
        // Best-effort counter bump; ignore if the session row isn't present.
        let _ = conn.execute(
            "UPDATE sessions SET message_count = message_count + 1,
             tool_call_count = tool_call_count + ?, last_activity_at = ?
             WHERE id = ?",
            params![num_tool_calls, ts, session_id],
        );
        Ok(id)
    }

    /// Persist one native assistant tool-call row or tool-result row before the
    /// loop advances. The transaction validates it against the current durable
    /// tail, preventing orphan results, duplicate ids, and reordered groups.
    pub fn append_native_tool_message(
        &self,
        session_id: &str,
        message: &Value,
        turn_lease_holder: Option<&str>,
    ) -> rusqlite::Result<bool> {
        if session_id.is_empty()
            || message
                .get("_empty_recovery_synthetic")
                .or_else(|| message.get("_empty_terminal_sentinel"))
                .is_some_and(crate::python_value::truthy)
        {
            return Ok(false);
        }
        let Some(object) = message.as_object() else {
            return Ok(false);
        };
        let Some(role @ ("assistant" | "tool")) = object.get("role").and_then(Value::as_str) else {
            return Ok(false);
        };

        let tool_calls = object
            .get("tool_calls")
            .filter(|value| !value.is_null())
            .map(Value::to_string);
        if role == "assistant"
            && object
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
        {
            return Ok(false);
        }
        if role == "assistant"
            && !matches!(
                phase_after_assistant(tool_calls.as_deref()),
                Some(TailPhase::Tools(pending)) if !pending.is_empty()
            )
        {
            return Ok(false);
        }
        let tool_call_id = object.get("tool_call_id").and_then(Value::as_str);

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(holder) = turn_lease_holder {
            let conversation_id = compression_lineage_root_on(&tx, session_id)?;
            let owner = tx
                .query_row(
                    "SELECT holder FROM session_turn_leases
                     WHERE conversation_id = ? AND expires_at >= ?",
                    params![conversation_id, now_secs()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if owner.as_deref() != Some(holder) {
                return Ok(false);
            }
        }
        let live = tx
            .query_row(
                "SELECT ended_at IS NULL FROM sessions WHERE id = ?",
                [session_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        if live != Some(true) {
            return Ok(false);
        }
        let tail = {
            let mut statement = tx.prepare(
                "SELECT role, tool_calls, tool_call_id FROM messages
                 WHERE session_id = ? AND active = 1
                   AND id >= COALESCE((
                       SELECT MAX(id) FROM messages
                       WHERE session_id = ? AND active = 1 AND role = 'user'
                   ), 0)
                 ORDER BY id",
            )?;
            let rows = statement
                .query_map(params![session_id, session_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let Some(phase) = partial_turn_phase(&tail) else {
            return Ok(false);
        };
        let allowed = match (role, phase) {
            ("assistant", TailPhase::Assistant) => true,
            ("assistant", TailPhase::Tools(pending)) => pending.is_empty(),
            ("tool", TailPhase::Tools(pending)) => tool_call_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .is_some_and(|id| pending.contains(id)),
            _ => false,
        };
        if !allowed {
            return Ok(false);
        }

        let content = native_persisted_content(role, object.get("content"));
        let api_content = object.get("api_content").and_then(Value::as_str);
        let tool_name = object
            .get("tool_name")
            .or_else(|| object.get("name"))
            .and_then(Value::as_str);
        let json_text = |key: &str| {
            object
                .get(key)
                .filter(|value| !value.is_null())
                .map(Value::to_string)
        };
        let timestamp = object
            .get("timestamp")
            .and_then(Value::as_f64)
            .unwrap_or_else(now_secs);
        let assistant_text = |key: &str| {
            (role == "assistant")
                .then(|| object.get(key).and_then(Value::as_str))
                .flatten()
        };
        let assistant_json = |key: &str| (role == "assistant").then(|| json_text(key)).flatten();
        tx.execute(
            "INSERT INTO messages (
                 session_id, role, content, api_content, tool_call_id, tool_calls,
                 tool_name, effect_disposition, finish_reason, reasoning,
                 reasoning_content, reasoning_details, codex_reasoning_items,
                 codex_message_items, timestamp, active
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)",
            params![
                session_id,
                role,
                content,
                api_content,
                tool_call_id,
                tool_calls,
                tool_name,
                object.get("effect_disposition").and_then(Value::as_str),
                assistant_text("finish_reason"),
                assistant_text("reasoning"),
                assistant_text("reasoning_content"),
                assistant_json("reasoning_details"),
                assistant_json("codex_reasoning_items"),
                assistant_json("codex_message_items"),
                timestamp,
            ],
        )?;
        let new_tool_calls = object
            .get("tool_calls")
            .and_then(Value::as_array)
            .map_or(0_i64, |calls| {
                i64::try_from(calls.len()).unwrap_or(i64::MAX)
            });
        tx.execute(
            "UPDATE sessions SET message_count = message_count + 1,
             tool_call_count = tool_call_count + ?, last_activity_at = ? WHERE id = ?",
            params![new_tool_calls, timestamp, session_id],
        )?;
        tx.commit()?;
        Ok(true)
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
            "SELECT role, content, api_content FROM messages
             WHERE session_id = ? AND active = 1 ORDER BY id ASC"
                .to_string()
        } else {
            format!(
                "SELECT role, content, api_content FROM (
                     SELECT id, role, content, api_content FROM messages
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
                api_content: r.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Capture the exact active row ids and model-facing fields used by a
    /// manual compression attempt. The max id is a commit watermark; rows
    /// appended by another process after this read are cloned into the child
    /// rather than summarized away.
    pub fn load_compression_snapshot(
        &self,
        session_id: &str,
    ) -> rusqlite::Result<CompressionSnapshot> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT id, role, content, api_content, tool_call_id, tool_calls, tool_name,
                    reasoning, reasoning_content, reasoning_details,
                    codex_reasoning_items, codex_message_items
             FROM messages
             WHERE session_id = ? AND active = 1 ORDER BY id ASC",
        )?;
        let messages = statement
            .query_map([session_id], |row| {
                Ok(CompressionHistoryMessage {
                    id: row.get(0)?,
                    message: HistoryMessage {
                        role: row.get(1)?,
                        content: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        api_content: row.get(3)?,
                    },
                    tool_call_id: row.get(4)?,
                    tool_calls: row.get(5)?,
                    tool_name: row.get(6)?,
                    reasoning: row.get(7)?,
                    reasoning_content: row.get(8)?,
                    reasoning_details: row.get(9)?,
                    codex_reasoning_items: row.get(10)?,
                    codex_message_items: row.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let watermark = messages.last().map_or(0, |message| message.id);
        Ok(CompressionSnapshot {
            watermark,
            messages,
        })
    }

    pub fn has_compression_checkpoint(&self, session_id: &str) -> rusqlite::Result<bool> {
        if session_id.is_empty() {
            return Ok(false);
        }
        self.conn.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM messages
             WHERE session_id = ? AND _compressed_summary = 1)",
            [session_id],
            |row| row.get(0),
        )
    }

    /// Reconstruct the durable active transcript for lifecycle hooks. Unlike
    /// model history, this keeps stored tool metadata and the clean/API content
    /// split so an end-of-session provider sees the best available transcript.
    pub fn load_lifecycle_messages(&self, session_id: &str) -> rusqlite::Result<Vec<Value>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT role, content, api_content, tool_call_id, tool_calls, tool_name,
                    effect_disposition, finish_reason, reasoning, reasoning_content,
                    reasoning_details, codex_reasoning_items, codex_message_items
             FROM messages WHERE session_id = ? AND active = 1 ORDER BY id ASC",
        )?;
        let rows = statement.query_map([session_id], |row| {
            let role: String = row.get(0)?;
            let content = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let api_content: Option<String> = row.get(2)?;
            let tool_call_id: Option<String> = row.get(3)?;
            let tool_calls: Option<String> = row.get(4)?;
            let tool_name: Option<String> = row.get(5)?;
            let effect_disposition: Option<String> = row.get(6)?;
            let finish_reason: Option<String> = row.get(7)?;
            let reasoning: Option<String> = row.get(8)?;
            let reasoning_content: Option<String> = row.get(9)?;
            let reasoning_details: Option<String> = row.get(10)?;
            let codex_reasoning_items: Option<String> = row.get(11)?;
            let codex_message_items: Option<String> = row.get(12)?;
            let mut message = serde_json::Map::new();
            message.insert("role".into(), Value::String(role.clone()));
            message.insert(
                "content".into(),
                HistoryMessage {
                    role: role.clone(),
                    content,
                    api_content: api_content.clone(),
                }
                .model_content(),
            );
            if let Some(api_content) = api_content {
                message.insert("api_content".into(), Value::String(api_content));
            }
            if let Some(tool_call_id) = tool_call_id {
                message.insert("tool_call_id".into(), Value::String(tool_call_id));
            }
            if let Some(tool_calls) = tool_calls {
                message.insert(
                    "tool_calls".into(),
                    serde_json::from_str(&tool_calls).unwrap_or(Value::String(tool_calls)),
                );
            }
            if let Some(tool_name) = tool_name {
                message.insert("name".into(), Value::String(tool_name.clone()));
                message.insert("tool_name".into(), Value::String(tool_name));
            }
            if let Some(effect_disposition) = effect_disposition {
                message.insert(
                    "effect_disposition".into(),
                    Value::String(effect_disposition),
                );
            }
            if role == "assistant" {
                if let Some(finish_reason) = finish_reason {
                    message.insert("finish_reason".into(), Value::String(finish_reason));
                }
                if let Some(reasoning) = reasoning {
                    message.insert("reasoning".into(), Value::String(reasoning));
                }
                if let Some(reasoning_content) = reasoning_content {
                    message.insert("reasoning_content".into(), Value::String(reasoning_content));
                }
                for (key, raw) in [
                    ("reasoning_details", reasoning_details),
                    ("codex_reasoning_items", codex_reasoning_items),
                    ("codex_message_items", codex_message_items),
                ] {
                    if let Some(raw) = raw {
                        message.insert(
                            key.into(),
                            serde_json::from_str(&raw).unwrap_or(Value::Null),
                        );
                    }
                }
            }
            Ok(Value::Object(message))
        })?;
        rows.collect()
    }

    /// Full-text search live and compression-archived messages across all
    /// sessions, newest-matching first by FTS rank. Rewind/undo rows remain
    /// hidden (`active=0, compacted=0`).
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
             WHERE messages_fts MATCH ?1 AND (m.active = 1 OR m.compacted = 1)
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
    fn tool_prune_publish_archives_and_clones_wide_rows_with_cas() {
        use super::{AppendOptions, GatewayToolPrunePublish, SessionDb};

        let path = temp_db("tool_prune_publish");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("prune", "local", None, None, None)
            .unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET model_config=? WHERE id='prune'",
                [r#"{"keep":"value"}"#],
            )
            .unwrap();
        db.append_message("prune", "user", "question").unwrap();
        let calls = r#"[{"id":"call-1","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"build\"}"}}]"#;
        db.append_message_with(
            "prune",
            "assistant",
            "",
            &AppendOptions {
                tool_calls: Some(calls),
                ..Default::default()
            },
        )
        .unwrap();
        db.append_message_with(
            "prune",
            "tool",
            &format!("{}\nexit_code: 0", "build output ".repeat(900)),
            &AppendOptions {
                tool_call_id: Some("call-1"),
                tool_name: Some("terminal"),
                display_kind: Some("terminal_result"),
                ..Default::default()
            },
        )
        .unwrap();
        db.append_message("prune", "assistant", "done").unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "UPDATE messages SET reasoning='preserve-me'
                 WHERE session_id='prune' AND role='tool'",
            )
            .unwrap();

        let snapshot = db.load_compression_snapshot("prune").unwrap();
        let candidate = crate::tool_result_prune::prune_old_tool_results(
            &snapshot.messages,
            1,
            crate::tool_result_prune::PRUNE_MIN_CHARS,
        );
        assert!(candidate.changed);
        assert!(db
            .publish_gateway_tool_prune(&GatewayToolPrunePublish {
                scope: "default",
                session_key: "peer",
                session_id: "prune",
                original_messages: &snapshot.messages,
                pruned_messages: &candidate.messages,
                rearm_tokens: 50_000,
                turn_lease_holder: None,
            })
            .unwrap());

        let active = db.load_compression_snapshot("prune").unwrap();
        assert_eq!(active.messages.len(), 4);
        assert!(active.messages[2].message.content.starts_with("[terminal]"));
        let conn = db.conn.lock().unwrap();
        let archived = conn
            .query_row(
                "SELECT COUNT(*) FROM messages
                 WHERE session_id='prune' AND active=0 AND compacted=1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(archived, 4);
        let preserved = conn
            .query_row(
                "SELECT reasoning, display_kind FROM messages
                 WHERE session_id='prune' AND active=1 AND role='tool'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        assert_eq!(preserved, ("preserve-me".into(), "terminal_result".into()));
        let config: serde_json::Value = conn
            .query_row(
                "SELECT model_config FROM sessions WHERE id='prune'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|raw| serde_json::from_str(&raw).unwrap())
            .unwrap();
        assert_eq!(config["keep"], "value");
        assert_eq!(config["_proactive_prune_rearm_tokens"], 50_000);
        drop(conn);

        // The old snapshot no longer matches the fresh active row ids. A stale
        // publisher is rejected without changing the committed transcript.
        assert!(!db
            .publish_gateway_tool_prune(&GatewayToolPrunePublish {
                scope: "default",
                session_key: "peer",
                session_id: "prune",
                original_messages: &snapshot.messages,
                pruned_messages: &candidate.messages,
                rearm_tokens: 99_000,
                turn_lease_holder: None,
            })
            .unwrap());
        assert_eq!(db.load_compression_snapshot("prune").unwrap(), active);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn provider_usage_updates_main_and_auxiliary_buckets_atomically() {
        use super::{SessionDb, UsageRoute};
        use crate::provider_usage::CanonicalUsage;

        let path = temp_db("provider_usage");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("usage", "local", None, None, None)
            .unwrap();
        let route = UsageRoute {
            model: "model-b",
            provider: "provider-b",
            base_url: "https://provider.invalid/v1",
            billing_mode: "metered",
        };
        let main = CanonicalUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_tokens: 5,
            cache_write_tokens: 3,
            reasoning_tokens: 2,
            request_count: 2,
        };
        assert!(db.record_main_usage("usage", &route, &main).unwrap());
        let auxiliary = CanonicalUsage {
            input_tokens: 13,
            output_tokens: 4,
            request_count: 1,
            ..CanonicalUsage::accumulator()
        };
        assert!(db
            .record_auxiliary_usage("usage", "compression", &route, &auxiliary)
            .unwrap());

        let conn = db.conn.lock().unwrap();
        let aggregate = conn
            .query_row(
                "SELECT input_tokens, output_tokens, cache_read_tokens,
                        cache_write_tokens, reasoning_tokens, api_call_count,
                        model, billing_provider
                 FROM sessions WHERE id='usage'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            aggregate,
            (11, 7, 5, 3, 2, 2, "model-b".into(), "provider-b".into())
        );
        let rows = conn
            .prepare(
                "SELECT task, input_tokens, output_tokens, api_call_count
                 FROM session_model_usage WHERE session_id='usage' ORDER BY task",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![("".into(), 11, 7, 2), ("compression".into(), 13, 4, 1)]
        );
        drop(conn);
        assert!(!db.record_main_usage("missing", &route, &main).unwrap());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn legacy_usage_primary_key_is_healed_on_open() {
        use super::{SessionDb, UsageRoute};
        use crate::provider_usage::CanonicalUsage;

        let path = temp_db("legacy_usage_primary_key");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("usage", "local", None, None, None)
            .unwrap();
        drop(db);

        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE session_model_usage;
             CREATE TABLE session_model_usage (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                billing_provider TEXT NOT NULL DEFAULT '',
                billing_base_url TEXT NOT NULL DEFAULT '',
                billing_mode TEXT NOT NULL DEFAULT '',
                api_call_count INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                estimated_cost_usd REAL NOT NULL DEFAULT 0,
                actual_cost_usd REAL NOT NULL DEFAULT 0,
                cost_status TEXT,
                cost_source TEXT,
                first_seen REAL,
                last_seen REAL,
                PRIMARY KEY (
                    session_id, model, billing_provider, billing_base_url,
                    billing_mode
                )
             );
             ALTER TABLE session_model_usage ADD COLUMN task TEXT;
             INSERT INTO session_model_usage (
                session_id, model, input_tokens, output_tokens, task
             ) VALUES ('usage', 'legacy-model', 10, 20, NULL);",
        )
        .unwrap();
        drop(conn);

        let db = SessionDb::open(path.clone()).unwrap();
        let route = UsageRoute {
            model: "new-model",
            provider: "provider",
            base_url: "https://provider.invalid/v1",
            billing_mode: "",
        };
        assert!(db
            .record_main_usage(
                "usage",
                &route,
                &CanonicalUsage {
                    input_tokens: 5,
                    request_count: 1,
                    ..CanonicalUsage::accumulator()
                },
            )
            .unwrap());
        let conn = db.conn.lock().unwrap();
        let mut primary = conn
            .prepare("PRAGMA table_info(session_model_usage)")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })
            .unwrap()
            .filter_map(Result::ok)
            .filter(|(_, position)| *position > 0)
            .collect::<Vec<_>>();
        primary.sort_by_key(|(_, position)| *position);
        assert_eq!(primary.last().unwrap().0, "task");
        let legacy = conn
            .query_row(
                "SELECT task, input_tokens FROM session_model_usage
                 WHERE session_id='usage' AND model='legacy-model'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert_eq!(legacy, ("".into(), 10));
        assert_eq!(
            conn.query_row(
                "SELECT input_tokens FROM session_model_usage
                 WHERE session_id='usage' AND model='new-model'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            5
        );
        assert!(conn
            .query_row(
                "SELECT 1 FROM sqlite_master
                 WHERE type='table' AND name='session_model_usage_legacy_pk'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .unwrap()
            .is_none());
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn usage_primary_key_heal_preserves_orphans_and_restores_foreign_keys() {
        use super::SessionDb;

        let path = temp_db("usage_primary_key_orphans");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("usage", "local", None, None, None)
            .unwrap();
        drop(db);
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute_batch(
            "DROP TABLE session_model_usage;
             CREATE TABLE session_model_usage (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                billing_provider TEXT NOT NULL DEFAULT '',
                billing_base_url TEXT NOT NULL DEFAULT '',
                billing_mode TEXT NOT NULL DEFAULT '',
                api_call_count INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                estimated_cost_usd REAL NOT NULL DEFAULT 0,
                actual_cost_usd REAL NOT NULL DEFAULT 0,
                cost_status TEXT,
                cost_source TEXT,
                first_seen REAL,
                last_seen REAL,
                PRIMARY KEY (
                    session_id, model, billing_provider, billing_base_url,
                    billing_mode
                )
             );
             INSERT INTO session_model_usage (session_id, model, input_tokens)
                VALUES ('usage', 'model', 1), ('orphan', 'model', 2);",
        )
        .unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        assert!(conn
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
            .unwrap());
        SessionDb::heal_session_model_usage_pk(&mut conn).unwrap();
        assert!(conn
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
            .unwrap());
        let sessions = conn
            .prepare("SELECT session_id FROM session_model_usage ORDER BY session_id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(sessions, ["orphan", "usage"]);
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn retained_tail_accepts_complete_tool_groups_and_rejects_dangling_calls() {
        let calls =
            Some(r#"[{"id":"call-a","type":"function"},{"id":"call-b","type":"function"}]"#.into());
        let complete = vec![
            ("user".into(), None, None),
            ("assistant".into(), calls.clone(), None),
            ("tool".into(), None, Some("call-b".into())),
            ("tool".into(), None, Some("call-a".into())),
            ("assistant".into(), None, None),
            ("user".into(), None, None),
            ("assistant".into(), None, None),
        ];
        assert!(super::complete_turn_sequence(&complete));

        let mut missing_tool = complete.clone();
        missing_tool.remove(3);
        assert!(!super::complete_turn_sequence(&missing_tool));

        let mut wrong_tool = complete;
        wrong_tool[2].2 = Some("unknown".into());
        assert!(!super::complete_turn_sequence(&wrong_tool));
    }

    #[test]
    fn in_place_compression_archives_head_and_clones_complete_tails() {
        use super::{AppendOptions, GatewayInPlaceCompressionPublish, HistoryMessage, SessionDb};
        let path = temp_db("in_place_compression");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("same", "local", None, None, None)
            .unwrap();
        for (role, content) in [
            ("user", "u0"),
            ("assistant", "a0"),
            ("user", "u1"),
            ("assistant", "a1"),
            ("user", "u2"),
            ("assistant", "a2"),
        ] {
            db.append_message("same", role, content).unwrap();
        }
        let snapshot = db.load_compression_snapshot("same").unwrap();
        db.append_message("same", "user", "concurrent").unwrap();
        let calls = r#"[{"id":"call-1","function":{"name":"terminal","arguments":"{}"}}]"#;
        db.append_message_with(
            "same",
            "assistant",
            "",
            &AppendOptions {
                tool_calls: Some(calls),
                ..Default::default()
            },
        )
        .unwrap();
        db.append_message_with(
            "same",
            "tool",
            "result",
            &AppendOptions {
                tool_call_id: Some("call-1"),
                tool_name: Some("terminal"),
                ..Default::default()
            },
        )
        .unwrap();
        db.append_message("same", "assistant", "done").unwrap();
        let compacted = [
            HistoryMessage {
                role: "user".into(),
                content: "summary".into(),
                api_content: None,
            },
            HistoryMessage {
                role: "assistant".into(),
                content: "waiting".into(),
                api_content: None,
            },
        ];

        assert!(db
            .publish_gateway_in_place_compression(&GatewayInPlaceCompressionPublish {
                scope: "scope",
                session_key: "route",
                session_id: "same",
                compacted_messages: &compacted,
                prefix_end_id: None,
                tail_start_id: Some(snapshot.messages[4].id),
                watermark: snapshot.watermark,
                turn_lease_holder: None,
            })
            .unwrap());
        let live = db.load_lifecycle_messages("same").unwrap();
        assert_eq!(
            live.iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "user",
                "assistant",
                "user",
                "assistant",
                "user",
                "assistant",
                "tool",
                "assistant"
            ]
        );
        assert_eq!(live[6]["tool_call_id"], "call-1");
        assert_eq!(live[6]["name"], "terminal");
        assert_eq!(
            db.conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT message_count, tool_call_count FROM sessions WHERE id='same'",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            (8, 1)
        );
        let before_failed_publish = live.clone();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_in_place_insert BEFORE INSERT ON messages
                 BEGIN SELECT RAISE(ABORT, 'injected in-place failure'); END;",
            )
            .unwrap();
        let failed_snapshot = db.load_compression_snapshot("same").unwrap();
        assert!(db
            .publish_gateway_in_place_compression(&GatewayInPlaceCompressionPublish {
                scope: "scope",
                session_key: "route",
                session_id: "same",
                compacted_messages: &compacted,
                prefix_end_id: None,
                tail_start_id: None,
                watermark: failed_snapshot.watermark,
                turn_lease_holder: None,
            })
            .is_err());
        assert_eq!(
            db.load_lifecycle_messages("same").unwrap(),
            before_failed_publish
        );
        let conn = db.conn.lock().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id='same' AND active=0 AND compacted=1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            4
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id='same' AND active=0 AND compacted=0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            6
        );
        drop(conn);
        assert!(db.get_session("same").unwrap().unwrap()["ended_at"].is_null());
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

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
    fn title_sanitizer_matches_python_controls_whitespace_and_limit() {
        assert_eq!(
            sanitize_session_title("  Project\t\n Phoenix  ").unwrap(),
            Some("Project Phoenix".into())
        );
        assert_eq!(
            sanitize_session_title("a\0\u{7}b\u{200b}c\u{202e}d\u{feff}e").unwrap(),
            Some("abcde".into())
        );
        assert_eq!(sanitize_session_title("\0\u{200b}").unwrap(), None);
        assert_eq!(
            sanitize_session_title(&"🦀".repeat(MAX_SESSION_TITLE_LENGTH)).unwrap(),
            Some("🦀".repeat(MAX_SESSION_TITLE_LENGTH))
        );
        assert!(
            sanitize_session_title(&"a".repeat(MAX_SESSION_TITLE_LENGTH + 1))
                .unwrap_err()
                .to_string()
                .contains("Title too long (101 chars, max 100)")
        );
    }

    #[test]
    fn manual_titles_are_unique_user_metadata_and_transfer_from_compressed_ancestor() {
        let path = temp_db("manual_titles");
        let db = SessionDb::open(path.clone()).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO sessions (id,source,title,title_source,started_at,ended_at,end_reason)
             VALUES ('root','local','Project','user',1,2,'compression');
             INSERT INTO sessions (id,source,parent_session_id,title,title_source,started_at)
             VALUES ('tip','local','root','Project #2','llm',3);
             INSERT INTO sessions (id,source,title,title_source,started_at)
             VALUES ('other','local','Other','llm',4);
             INSERT INTO sessions (id,source,title,title_source,hidden,started_at)
             VALUES ('bot','local','Bot Chat','user',1,5);",
            )
            .unwrap();

        assert!(db.set_user_session_title("tip", "Project").unwrap());
        assert_eq!(db.get_session_title("root").unwrap(), None);
        assert_eq!(
            db.get_session_title("tip").unwrap().as_deref(),
            Some("Project")
        );
        assert_eq!(
            db.get_session("tip").unwrap().unwrap()["title_source"],
            "user"
        );

        let error = db.set_user_session_title("other", "Project").unwrap_err();
        assert!(matches!(error, SetSessionTitleError::Rejected(_)));
        assert_eq!(
            db.get_session_title("other").unwrap().as_deref(),
            Some("Other")
        );
        assert!(db
            .set_user_session_title("bot", "Renamed")
            .unwrap_err()
            .to_string()
            .contains("canonical Bot Chat"));
        assert!(db.set_user_session_title("other", "").unwrap());
        assert_eq!(db.get_session_title("other").unwrap(), None);
        assert!(db.get_session("other").unwrap().unwrap()["title_source"].is_null());
        assert!(!db.set_user_session_title("missing", "Missing").unwrap());
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn title_index_repairs_legacy_duplicates_and_then_enforces_uniqueness() {
        let path = temp_db("title_index_repair");
        let db = SessionDb::open(path.clone()).unwrap();
        db.ensure_session("older", "local", None, None, None)
            .unwrap();
        db.ensure_session("newer", "local", None, None, None)
            .unwrap();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute("DROP INDEX idx_sessions_title_unique", [])
                .unwrap();
            conn.execute(
                "UPDATE sessions SET title='shared' WHERE id IN ('older','newer')",
                [],
            )
            .unwrap();
        }
        drop(db);

        let reopened = SessionDb::open(path.clone()).unwrap();
        assert_eq!(
            reopened
                .conn
                .lock()
                .unwrap()
                .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, u64>(0))
                .unwrap(),
            5_000
        );
        assert_eq!(reopened.get_session_title("older").unwrap(), None);
        assert_eq!(
            reopened.get_session_title("newer").unwrap().as_deref(),
            Some("shared")
        );
        assert!(reopened.set_user_session_title("older", "shared").is_err());
        drop(reopened);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_manual_title_claim_has_one_winner() {
        let path = temp_db("title_race");
        let first = SessionDb::open(path.clone()).unwrap();
        first
            .ensure_session("one", "local", None, None, None)
            .unwrap();
        first
            .ensure_session("two", "local", None, None, None)
            .unwrap();
        let second = SessionDb::open(path.clone()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [(first, "one"), (second, "two")]
            .into_iter()
            .map(|(database, id)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    database.set_user_session_title(id, "claimed")
                })
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Ok(true)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(SetSessionTitleError::Rejected(_))))
                .count(),
            1
        );
        let verify = SessionDb::open(path.clone()).unwrap();
        let titled = ["one", "two"]
            .into_iter()
            .filter(|id| verify.get_session_title(id).unwrap().as_deref() == Some("claimed"))
            .count();
        assert_eq!(titled, 1);
        drop(verify);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
        assert_eq!(db.compression_lineage_root("message").unwrap(), "root");
        assert_eq!(db.compression_lineage_root("branch").unwrap(), "branch");
        assert_eq!(db.compression_lineage_root("delegate").unwrap(), "delegate");
        assert_eq!(db.compression_lineage_root("tool").unwrap(), "tool");
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
    fn compression_publish_is_child_first_atomic_and_clones_tail_rows() {
        let path = temp_db("compression_publish");
        let db = SessionDb::open(path.clone()).unwrap();
        db.create_session(
            "parent",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "local",
                    session_key: Some("route"),
                    user_id: Some("user"),
                    chat_id: Some("chat"),
                    chat_type: Some("dm"),
                    thread_id: None,
                },
                system_prompt: Some("frozen prompt"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(db
            .set_user_session_title("parent", "Compression Project")
            .unwrap());
        for (role, content) in [
            ("user", "u0"),
            ("assistant", "a0"),
            ("user", "u1"),
            ("assistant", "a1"),
            ("user", "u2"),
            ("assistant", "a2"),
        ] {
            db.append_message("parent", role, content).unwrap();
        }
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE messages SET tool_calls = ?, tool_name = ?
                 WHERE session_id = 'parent' AND role = 'assistant' AND content = 'a0'",
                params![
                    r#"[{"id":"call-1","function":{"name":"terminal","arguments":"{}"}}]"#,
                    "terminal"
                ],
            )
            .unwrap();
        let snapshot = db.load_compression_snapshot("parent").unwrap();
        assert!(snapshot.messages[1]
            .tool_calls
            .as_deref()
            .unwrap()
            .contains("call-1"));
        assert_eq!(snapshot.messages[1].tool_name.as_deref(), Some("terminal"));
        db.append_message("parent", "user", "concurrent").unwrap();
        db.append_message("parent", "assistant", "concurrent answer")
            .unwrap();
        let compacted = [
            HistoryMessage {
                role: "user".into(),
                content: "summary".into(),
                api_content: None,
            },
            HistoryMessage {
                role: "assistant".into(),
                content: "waiting".into(),
                api_content: None,
            },
        ];
        let entry = serde_json::json!({
            "session_key":"route", "session_id":"child",
            "created_at":"2026-09-08T00:00:00", "updated_at":"2026-09-08T00:00:00"
        })
        .to_string();
        assert!(db
            .publish_gateway_compression(&GatewayCompressionPublish {
                scope: "scope",
                session_key: "route",
                entry_json: &entry,
                parent_id: "parent",
                child_id: "child",
                compacted_messages: &compacted,
                prefix_end_id: None,
                tail_start_id: Some(snapshot.messages[4].id),
                watermark: snapshot.watermark,
                turn_lease_holder: None,
            })
            .unwrap());
        assert_eq!(
            db.load_history("child", 0)
                .unwrap()
                .into_iter()
                .map(|message| (message.role, message.content))
                .collect::<Vec<_>>(),
            [
                ("user".into(), "summary".into()),
                ("assistant".into(), "waiting".into()),
                ("user".into(), "u2".into()),
                ("assistant".into(), "a2".into()),
                ("user".into(), "concurrent".into()),
                ("assistant".into(), "concurrent answer".into()),
            ]
        );
        assert_eq!(
            db.get_session("parent").unwrap().unwrap()["end_reason"],
            "compression"
        );
        assert_eq!(
            db.get_session("child").unwrap().unwrap()["parent_session_id"],
            "parent"
        );
        assert_eq!(
            db.get_session("child").unwrap().unwrap()["system_prompt"],
            "frozen prompt"
        );
        assert_eq!(
            db.get_session("child").unwrap().unwrap()["title"],
            "Compression Project"
        );
        assert_eq!(
            db.get_session("child").unwrap().unwrap()["title_source"],
            "user"
        );
        assert!(db.get_session("parent").unwrap().unwrap()["title"].is_null());
        assert!(db.load_gateway_routing_entries("scope").unwrap()["route"].contains("child"));

        for id in ["stale-parent", "resumed-target"] {
            db.create_session(
                id,
                &SessionCreate {
                    peer: GatewayPeer {
                        source: "local",
                        session_key: Some("stale-route"),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        }
        db.append_message("stale-parent", "user", "do not rotate")
            .unwrap();
        let stale = db.load_compression_snapshot("stale-parent").unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO gateway_routing(scope,session_key,entry_json,updated_at)
                 VALUES ('scope','stale-route','{\"session_id\":\"resumed-target\"}',1)",
                [],
            )
            .unwrap();
        assert!(!db
            .publish_gateway_compression(&GatewayCompressionPublish {
                scope: "scope",
                session_key: "stale-route",
                entry_json: "{\"session_id\":\"stale-child\"}",
                parent_id: "stale-parent",
                child_id: "stale-child",
                compacted_messages: &compacted,
                prefix_end_id: None,
                tail_start_id: None,
                watermark: stale.watermark,
                turn_lease_holder: None,
            })
            .unwrap());
        assert!(db.get_session("stale-child").unwrap().is_none());
        assert!(db.get_session("stale-parent").unwrap().unwrap()["ended_at"].is_null());
        assert!(
            db.load_gateway_routing_entries("scope").unwrap()["stale-route"]
                .contains("resumed-target")
        );

        db.create_session(
            "incomplete-parent",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "local",
                    session_key: Some("incomplete-route"),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        db.append_message("incomplete-parent", "user", "complete question")
            .unwrap();
        db.append_message("incomplete-parent", "assistant", "complete answer")
            .unwrap();
        let incomplete = db.load_compression_snapshot("incomplete-parent").unwrap();
        db.append_message("incomplete-parent", "user", "still in flight")
            .unwrap();
        assert!(!db
            .publish_gateway_compression(&GatewayCompressionPublish {
                scope: "scope",
                session_key: "incomplete-route",
                entry_json: "{\"session_id\":\"incomplete-child\"}",
                parent_id: "incomplete-parent",
                child_id: "incomplete-child",
                compacted_messages: &compacted,
                prefix_end_id: None,
                tail_start_id: None,
                watermark: incomplete.watermark,
                turn_lease_holder: None,
            })
            .unwrap());
        assert!(db.get_session("incomplete-child").unwrap().is_none());
        assert!(db.get_session("incomplete-parent").unwrap().unwrap()["ended_at"].is_null());

        db.create_session(
            "rollback-parent",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "local",
                    session_key: Some("rollback-route"),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        db.append_message("rollback-parent", "user", "keep")
            .unwrap();
        let rollback = db.load_compression_snapshot("rollback-parent").unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_compression_handoff BEFORE INSERT ON messages
             WHEN new.session_id = 'rollback-child' AND new.role = 'assistant'
             BEGIN SELECT RAISE(ABORT, 'fixture handoff failure'); END;",
            )
            .unwrap();
        let failed = db.publish_gateway_compression(&GatewayCompressionPublish {
            scope: "scope",
            session_key: "rollback-route",
            entry_json: "{}",
            parent_id: "rollback-parent",
            child_id: "rollback-child",
            compacted_messages: &compacted,
            prefix_end_id: None,
            tail_start_id: None,
            watermark: rollback.watermark,
            turn_lease_holder: None,
        });
        assert!(failed.is_err());
        assert!(db.get_session("rollback-child").unwrap().is_none());
        assert!(db.get_session("rollback-parent").unwrap().unwrap()["ended_at"].is_null());
        assert_eq!(db.load_history("rollback-parent", 0).unwrap().len(), 1);
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_compression_publications_have_exactly_one_winner() {
        let path = temp_db("compression_competing");
        let db = SessionDb::open(path.clone()).unwrap();
        db.create_session(
            "parent",
            &SessionCreate {
                peer: GatewayPeer {
                    source: "local",
                    session_key: Some("route"),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        db.append_message("parent", "user", "question").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers = ["child-a", "child-b"]
            .into_iter()
            .map(|child| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let db = SessionDb::open(path).unwrap();
                    let entry = format!("{{\"session_id\":\"{child}\"}}");
                    let compacted = [
                        HistoryMessage {
                            role: "user".into(),
                            content: format!("summary-{child}"),
                            api_content: None,
                        },
                        HistoryMessage {
                            role: "assistant".into(),
                            content: "waiting".into(),
                            api_content: None,
                        },
                    ];
                    barrier.wait();
                    db.publish_gateway_compression(&GatewayCompressionPublish {
                        scope: "scope",
                        session_key: "route",
                        entry_json: &entry,
                        parent_id: "parent",
                        child_id: child,
                        compacted_messages: &compacted,
                        prefix_end_id: None,
                        tail_start_id: None,
                        watermark: 1,
                        turn_lease_holder: None,
                    })
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|won| **won).count(), 1);
        let children: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE parent_session_id = 'parent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(children, 1);
        assert_eq!(db.get_compression_chain("parent").unwrap().len(), 2);
        let route = &db.load_gateway_routing_entries("scope").unwrap()["route"];
        assert!(route.contains("child-a") || route.contains("child-b"));
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
    fn resume_catalog_resolves_ids_titles_lineage_and_exact_lane() {
        let path = temp_db("resume_catalog");
        let db = SessionDb::open(path.clone()).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO sessions
             (id,source,user_id,session_key,chat_id,chat_type,title,started_at,last_activity_at)
             VALUES
             ('base','telegram','U','lane','C','dm','Plan_%',1,1),
             ('next','telegram','U','lane','C','dm','Plan_% #2',2,2),
             ('foreign','telegram','U','other','D','dm','Foreign',3,3);
             INSERT INTO messages(session_id,role,content,timestamp)
             VALUES ('next','user','first work',4),
                    ('next','user','later work',5);",
            )
            .unwrap();
        assert_eq!(
            db.resolve_session_target("base").unwrap().as_deref(),
            Some("base")
        );
        assert_eq!(
            db.resolve_session_target("Plan_%").unwrap().as_deref(),
            Some("next")
        );
        assert!(db.resolve_session_target("PlanXX").unwrap().is_none());
        let rows = db
            .list_resume_sessions(Some("telegram"), Some("lane"), false, 10)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], "next");
        assert_eq!(rows[0]["preview"], "first work");
        assert_eq!(db.user_message_count("next").unwrap(), 2);
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn resume_transaction_rolls_back_every_durable_change() {
        let path = temp_db("resume_rollback");
        let db = SessionDb::open(path.clone()).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO sessions
             (id,source,user_id,session_key,chat_id,chat_type,started_at,last_activity_at)
             VALUES
             ('outgoing','telegram','U','lane','C','dm',1,1),
             ('target','telegram','U','lane','C','dm',2,2);
             UPDATE sessions SET ended_at = 3, end_reason = 'agent_close' WHERE id = 'target';
             INSERT INTO gateway_routing(scope,session_key,entry_json,updated_at)
             VALUES ('scope','lane','old-route',1);
             CREATE TRIGGER reject_resume_route BEFORE INSERT ON gateway_routing BEGIN
                 SELECT RAISE(ABORT, 'forced route failure');
             END;",
            )
            .unwrap();

        let change = GatewaySessionSwitch {
            scope: "scope",
            session_key: "lane",
            entry_json: "new-route",
            outgoing_id: "outgoing",
            target_id: "target",
            peer: GatewayPeer {
                source: "telegram",
                session_key: Some("lane"),
                user_id: Some("U"),
                chat_id: Some("C"),
                chat_type: Some("dm"),
                thread_id: None,
            },
            display_name: None,
            origin_json: None,
        };
        assert!(db.switch_gateway_session(&change).is_err());

        let outgoing = db.get_session("outgoing").unwrap().unwrap();
        assert!(outgoing["ended_at"].is_null());
        assert!(outgoing["end_reason"].is_null());
        let target = db.get_session("target").unwrap().unwrap();
        assert_eq!(target["ended_at"], 3.0);
        assert_eq!(target["end_reason"], "agent_close");
        let conn = db.conn.lock().unwrap();
        let route: String = conn
            .query_row(
                "SELECT entry_json FROM gateway_routing WHERE scope = 'scope' AND session_key = 'lane'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(route, "old-route");
        let generations: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversation_generations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(generations, 0);
        drop(conn);
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
                    content: "hello".into(),
                    api_content: None,
                },
                HistoryMessage {
                    role: "assistant".into(),
                    content: "hi there".into(),
                    api_content: None,
                },
                HistoryMessage {
                    role: "user".into(),
                    content: "how are you".into(),
                    api_content: None,
                },
            ]
        );
        assert_eq!(db.message_count("s1").unwrap(), 3);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn api_content_is_checkpointed_on_the_matching_user_row_and_replayed() {
        let path = temp_db("api_content_sidecar");
        let db = SessionDb::open(path).unwrap();
        db.ensure_session("s1", "cli", None, None, None).unwrap();
        db.append_message("s1", "user", "first").unwrap();
        db.append_message("s1", "assistant", "answer").unwrap();
        let current = serde_json::json!("second");
        let current_id = db.append_message("s1", "user", "second").unwrap();

        assert!(!db
            .set_latest_user_api_content("s1", &serde_json::json!("first"), "must not attach")
            .unwrap());
        assert!(db
            .set_latest_user_api_content(
                "s1",
                &current,
                "second\n\n<memory-context>recalled</memory-context>"
            )
            .unwrap());
        assert_eq!(db.user_turn_count("s1").unwrap(), 2);
        assert_eq!(
            db.get_message(current_id).unwrap().unwrap().content,
            "second"
        );
        let restored = db.load_history("s1", 0).unwrap();
        assert_eq!(restored[2].content, "second");
        assert_eq!(
            restored[2].api_content.as_deref(),
            Some("second\n\n<memory-context>recalled</memory-context>")
        );
    }

    #[test]
    fn lifecycle_messages_preserve_clean_content_sidecar_and_tool_metadata() {
        let path = temp_db("lifecycle_messages");
        let db = super::SessionDb::open(path).unwrap();
        db.ensure_session("s1", "local", None, Some("C"), Some("dm"))
            .unwrap();
        let clean = serde_json::json!([
            {"type":"text","text":"look"},
            {"type":"image_url","image_url":{"url":"https://fixture/image.png"}}
        ]);
        db.append_message("s1", "user", &super::encode_message_content(&clean))
            .unwrap();
        db.set_latest_user_api_content("s1", &clean, "look\n\nRelevant memory")
            .unwrap();
        let calls = serde_json::json!([{
            "id":"call-1","type":"function",
            "function":{"name":"fixture_tool","arguments":"{\"x\":1}"}
        }]);
        db.append_message_with(
            "s1",
            "assistant",
            "",
            &super::AppendOptions {
                tool_calls: Some(&calls.to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        db.append_message_with(
            "s1",
            "tool",
            "tool result",
            &super::AppendOptions {
                tool_call_id: Some("call-1"),
                tool_name: Some("fixture_tool"),
                ..Default::default()
            },
        )
        .unwrap();
        let inactive = db.append_message("s1", "assistant", "inactive").unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE messages SET active=0 WHERE id=?", [inactive])
            .unwrap();

        assert_eq!(
            db.load_lifecycle_messages("s1").unwrap(),
            vec![
                serde_json::json!({
                    "role":"user",
                    "content":clean,
                    "api_content":"look\n\nRelevant memory"
                }),
                serde_json::json!({"role":"assistant","content":"","tool_calls":calls}),
                serde_json::json!({
                    "role":"tool","content":"tool result",
                    "tool_call_id":"call-1","name":"fixture_tool",
                    "tool_name":"fixture_tool"
                }),
            ]
        );
    }

    #[test]
    fn expiry_marker_and_reset_boundary_commit_atomically_and_idempotently() {
        let path = temp_db("expiry_boundary");
        let db = super::SessionDb::open(path).unwrap();
        db.create_session(
            "expiring",
            &super::SessionCreate {
                peer: super::GatewayPeer {
                    source: "telegram",
                    session_key: Some("agent:main:telegram:dm:C"),
                    chat_id: Some("C"),
                    chat_type: Some("dm"),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        db.finalize_session_expiry("expiring").unwrap();
        db.finalize_session_expiry("expiring").unwrap();
        let row = db.get_session("expiring").unwrap().unwrap();
        assert_eq!(row["expiry_finalized"], 1);
        assert_eq!(row["end_reason"], "session_reset");
        let generations: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT generation FROM conversation_generations WHERE source='telegram' AND session_key='agent:main:telegram:dm:C'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(generations, 1);

        db.create_session(
            "explicit",
            &super::SessionCreate {
                peer: super::GatewayPeer {
                    source: "telegram",
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        db.end_session("explicit", "compression").unwrap();
        db.finalize_session_expiry("explicit").unwrap();
        let explicit = db.get_session("explicit").unwrap().unwrap();
        assert_eq!(explicit["expiry_finalized"], 1);
        assert_eq!(explicit["end_reason"], "compression");
    }

    #[test]
    fn opening_a_legacy_message_table_adds_the_api_content_sidecar() {
        let path = temp_db("api_content_migration");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT,
                tool_name TEXT, display_kind TEXT, display_metadata TEXT,
                timestamp REAL NOT NULL, active INTEGER NOT NULL DEFAULT 1
             );
             INSERT INTO messages (session_id, role, content, timestamp)
             VALUES ('legacy', 'user', 'clean', 1);
             CREATE VIRTUAL TABLE messages_fts USING fts5(
                content, tool_name, tool_calls,
                content='messages', content_rowid='id'
             );
             INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');",
        )
        .unwrap();
        drop(conn);

        let db = SessionDb::open(path).unwrap();
        assert!(db
            .set_latest_user_api_content(
                "legacy",
                &serde_json::json!("clean"),
                "clean with recalled context",
            )
            .unwrap());
        let restored = db.load_history("legacy", 0).unwrap();
        assert_eq!(restored[0].content, "clean");
        assert_eq!(
            restored[0].api_content.as_deref(),
            Some("clean with recalled context")
        );
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
            api_content: None,
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
            api_content: None,
        };
        assert_eq!(hm.model_content(), parts);

        let obj = serde_json::json!({"type": "text", "text": "solo"});
        let hm2 = HistoryMessage {
            role: "user".into(),
            content: encode_message_content(&obj),
            api_content: None,
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
            api_content: None,
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
                api_content: None,
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

    #[test]
    fn compression_guard_state_merges_clears_and_persists_across_connections() {
        let path = temp_db("compression_guard");
        // Two independent connections onto the same file, as a resumed
        // compressor in another process would see it.
        let writer = SessionDb::open(path.clone()).unwrap();
        let reader = SessionDb::open(path.clone()).unwrap();
        writer.ensure_session("s", "cli", None, None, None).unwrap();

        // Fresh row: exists, cooldown NULL, breaker at rest.
        let state = reader.load_compression_guard_state("s").unwrap();
        assert!(state.session_exists);
        assert_eq!(state.cooldown_until, None);
        assert_eq!(state.cooldown_error, None);
        assert_eq!(state.ineffective_count, 0);
        assert_eq!(state.recovery_deadline, 0.0);

        // Record a cooldown, then a shorter one: merge-max keeps the longer
        // deadline while the latest error always wins.
        assert!(writer
            .record_compression_failure_cooldown("s", 500.0, Some("first"))
            .unwrap());
        assert!(writer
            .record_compression_failure_cooldown("s", 200.0, Some("second"))
            .unwrap());
        let state = reader.load_compression_guard_state("s").unwrap();
        assert_eq!(state.cooldown_until, Some(500.0));
        assert_eq!(state.cooldown_error.as_deref(), Some("second"));

        // A longer deadline does extend it; error still replaced.
        assert!(writer
            .record_compression_failure_cooldown("s", 900.0, None)
            .unwrap());
        let state = reader.load_compression_guard_state("s").unwrap();
        assert_eq!(state.cooldown_until, Some(900.0));
        assert_eq!(state.cooldown_error, None);

        // The load exposes the exact stored deadline even when it is already in
        // the past; expiry interpretation belongs to the caller.
        assert!(writer
            .record_compression_failure_cooldown("s", 1.0, Some("expired"))
            .unwrap());
        assert!(writer.clear_compression_failure_cooldown("s").unwrap());
        // Re-arm with a stale deadline and confirm it is returned verbatim.
        assert!(writer
            .record_compression_failure_cooldown("s", 1.0, Some("stale"))
            .unwrap());
        let state = reader.load_compression_guard_state("s").unwrap();
        assert_eq!(state.cooldown_until, Some(1.0));

        // Exact clearing zeroes both cooldown columns.
        assert!(writer.clear_compression_failure_cooldown("s").unwrap());
        let state = reader.load_compression_guard_state("s").unwrap();
        assert_eq!(state.cooldown_until, None);
        assert_eq!(state.cooldown_error, None);

        // Breaker values normalize nonnegative; negative inputs clamp to zero.
        assert!(writer.set_compression_breaker("s", -3, -1.0).unwrap());
        let state = reader.load_compression_guard_state("s").unwrap();
        assert_eq!(state.ineffective_count, 0);
        assert_eq!(state.recovery_deadline, 0.0);
        let raw_deadline = reader
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT compression_recovery_deadline FROM sessions WHERE id='s'",
                [],
                |row| row.get::<_, Option<f64>>(0),
            )
            .unwrap();
        assert_eq!(raw_deadline, None);
        assert!(writer.set_compression_breaker("s", 4, 1234.5).unwrap());
        let state = reader.load_compression_guard_state("s").unwrap();
        assert_eq!(state.ineffective_count, 4);
        assert_eq!(state.recovery_deadline, 1234.5);

        // Missing session: every update reports false, load reports absence.
        assert!(!writer
            .record_compression_failure_cooldown("missing", 10.0, Some("x"))
            .unwrap());
        assert!(!writer
            .clear_compression_failure_cooldown("missing")
            .unwrap());
        assert!(!writer.set_compression_breaker("missing", 1, 5.0).unwrap());
        assert!(
            !reader
                .load_compression_guard_state("missing")
                .unwrap()
                .session_exists
        );

        // Empty id fails closed on both read and write, with no row created.
        assert!(!writer
            .record_compression_failure_cooldown("", 10.0, None)
            .unwrap());
        assert!(!writer.clear_compression_failure_cooldown("").unwrap());
        assert!(!writer.set_compression_breaker("", 1, 5.0).unwrap());
        assert!(
            !reader
                .load_compression_guard_state("")
                .unwrap()
                .session_exists
        );

        // Reopen from disk: breaker survives, cleared cooldown stays cleared.
        drop(writer);
        drop(reader);
        let reopened = SessionDb::open(path.clone()).unwrap();
        let state = reopened.load_compression_guard_state("s").unwrap();
        assert!(state.session_exists);
        assert_eq!(state.cooldown_until, None);
        assert_eq!(state.ineffective_count, 4);
        assert_eq!(state.recovery_deadline, 1234.5);
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
                api_content: None,
            };
            assert_eq!(from_python.model_content(), case["decoded"], "{case}");
            if let Some(input) = case.get("input") {
                let from_rust = HistoryMessage {
                    role: "user".into(),
                    content: encode_message_content(input),
                    api_content: None,
                };
                assert_eq!(from_rust.model_content(), case["decoded"], "{case}");
            }
        }
    }
}
