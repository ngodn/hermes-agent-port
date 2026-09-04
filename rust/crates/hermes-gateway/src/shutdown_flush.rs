//! Port of the data-loss flush/recover core of gateway/shutdown_flush.py.
//!
// Public API is ahead of its callers (the shutdown + startup-recovery paths).
#![allow(dead_code)]
//!
//! Flush pending messages and agent transcripts to disk before shutdown so a
//! teardown (or FTS/SQLite corruption that blocks `INSERT INTO messages`) can't
//! silently discard the only surviving copy of user data (#72680).
//!
//!  * `flush_pending_to_file` — serialize non-empty pending slots to private
//!    JSON files under `<HERMES_HOME>/pending_messages/` before the in-memory
//!    map is cleared.
//!  * `recover_pending_to_db` — on startup, read those files and insert the
//!    messages via `SessionDb::append_message_with` (so FTS + session metadata
//!    are handled), deleting each file on success.
//!  * `flush_agent_history_to_file` — dump a live agent transcript to the same
//!    recovery directory when the normal DB flush raises (operator salvage).
//!
//! Best-effort: a backup failure must never block shutdown.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::session_db::{AppendOptions, SessionDb};

/// Reason tag on a cap-dropped transcript spool payload (#78182).
pub const TRANSCRIPT_CAP_DROP_REASON: &str = "transcript_cap_drop";
const AGENT_HISTORY_REASON: &str = "shutdown-with-unpersisted-agent-history";

fn flush_dir(home: Option<&Path>) -> PathBuf {
    let base = home
        .map(|h| h.to_path_buf())
        .unwrap_or_else(crate::config_file::hermes_home);
    base.join("pending_messages")
}

fn ensure_flush_dir(home: Option<&Path>) -> Option<PathBuf> {
    let dir = flush_dir(home);
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Atomically write one private (0600), uniquely named recovery payload.
fn write_payload(dir: &Path, payload: &Value) -> Option<PathBuf> {
    let file_id = format!("{:x}{:x}", std::process::id(), now_nanos());
    let final_path = dir.join(format!("pending-{file_id}.json"));
    let tmp = dir.join(format!("pending-{file_id}.json.tmp"));
    let bytes = serde_json::to_vec(payload).ok()?;
    write_private(&tmp, &bytes)?;
    std::fs::rename(&tmp, &final_path).ok()?;
    fsync_dir(dir);
    Some(final_path)
}

fn write_private(path: &Path, bytes: &[u8]) -> Option<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .ok()?;
        f.write_all(bytes).ok()?;
        Some(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).ok()
    }
}

fn fsync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(f) = std::fs::File::open(dir) {
            let _ = f.sync_all();
        }
    }
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Convert a pending value to a JSON object payload: a string becomes
/// `{"text": ...}`, an object is kept as-is, anything else stringifies.
fn serialise_value(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(json!({ "text": s })),
        Value::Object(_) => Some(value.clone()),
        other => Some(json!({ "text": other.to_string() })),
    }
}

/// Serialize non-empty pending slots to disk. Returns the number flushed.
/// `pending` is the `(session_key, value)` set from the runner/adapter pending
/// map; a `value` is typically a string or a message object.
pub fn flush_pending_to_file(
    pending: &[(String, Value)],
    reason: &str,
    home: Option<&Path>,
) -> usize {
    if pending.is_empty() {
        return 0;
    }
    let Some(dir) = ensure_flush_dir(home) else {
        return 0;
    };
    let ts = now_secs();
    let mut flushed = 0;
    for (session_key, value) in pending {
        let Some(serialised) = serialise_value(value) else {
            continue;
        };
        let payload = json!({
            "session_key": session_key,
            "reason": reason,
            "ts": ts,
            "data": serialised,
        });
        if write_payload(&dir, &payload).is_some() {
            flushed += 1;
        }
    }
    if flushed > 0 {
        tracing::info!(flushed, reason, dir = %dir.display(), "flushed pending messages");
    }
    flushed
}

/// Dump an agent's in-memory transcript to the recovery directory when the
/// normal DB flush raises. For manual operator salvage; `recover_pending_to_db`
/// deliberately skips these. Best-effort.
pub fn flush_agent_history_to_file(
    session_id: Option<&str>,
    history: &[Value],
    home: Option<&Path>,
) {
    if history.is_empty() {
        return;
    }
    let Some(dir) = ensure_flush_dir(home) else {
        return;
    };
    let payload = json!({
        "reason": AGENT_HISTORY_REASON,
        "issue": "#72680",
        "session_id": session_id,
        "count": history.len(),
        "messages": history,
    });
    if write_payload(&dir, &payload).is_some() {
        tracing::warn!(
            count = history.len(),
            session_id = session_id.unwrap_or(""),
            "preserved in-memory messages (recover after repairing state.db)"
        );
    }
}

/// Recover flushed pending messages into `db`, deleting each file on success.
/// Returns the number of messages recovered.
pub fn recover_pending_to_db(db: &SessionDb, home: Option<&Path>) -> usize {
    let dir = flush_dir(home);
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect(),
        Err(_) => return 0,
    };
    files.sort();

    let mut recovered = 0;
    for path in files {
        let Some(payload) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("");

        // Agent-history snapshots are for manual recovery, not auto-insert.
        if reason == AGENT_HISTORY_REASON {
            continue;
        }

        // Cap-dropped transcript payloads carry the full message dict.
        if reason == TRANSCRIPT_CAP_DROP_REASON {
            let data = payload.get("data").cloned().unwrap_or(Value::Null);
            let sid = data.get("session_id").and_then(Value::as_str).unwrap_or("");
            let message = data.get("message");
            let (Some(message), false) = (message.filter(|m| m.is_object()), sid.is_empty()) else {
                tracing::warn!(path = %path.display(), "invalid transcript spool file; preserved");
                continue;
            };
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let content = message.get("content").and_then(Value::as_str).unwrap_or("");
            let ts = message
                .get("timestamp")
                .and_then(Value::as_f64)
                .or_else(|| payload.get("ts").and_then(Value::as_f64));
            if db
                .append_message_with(
                    sid,
                    role,
                    content,
                    &AppendOptions {
                        timestamp: ts,
                        ..Default::default()
                    },
                )
                .is_ok()
            {
                recovered += 1;
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }

        // Plain pending message.
        let session_key = payload
            .get("session_key")
            .and_then(Value::as_str)
            .unwrap_or("");
        let data = payload.get("data").cloned().unwrap_or(Value::Null);
        let text = data.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() || session_key.is_empty() {
            tracing::warn!(path = %path.display(), "invalid pending message; preserved");
            continue;
        }
        // We need the real session_id to append; the gateway routing key alone
        // can't be resolved here, so require the serialised session_id (as the
        // Python does) and otherwise preserve the file.
        let session_id = data.get("session_id").and_then(Value::as_str).unwrap_or("");
        if session_id.is_empty() {
            tracing::warn!(%session_key, path = %path.display(), "no session_id in flush file; preserved");
            continue;
        }
        // Insert against the real session_id with the recorded timestamp.
        let ts = payload.get("ts").and_then(Value::as_f64);
        if db
            .append_message_with(
                session_id,
                "user",
                text,
                &AppendOptions {
                    timestamp: ts,
                    ..Default::default()
                },
            )
            .is_ok()
        {
            recovered += 1;
            let _ = std::fs::remove_file(&path);
        }
    }
    if recovered > 0 {
        tracing::info!(recovered, "recovered pending messages from shutdown flush");
    }
    recovered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_flush_{}_{}_{}",
            tag,
            std::process::id(),
            now_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn open_db(home: &Path) -> SessionDb {
        SessionDb::open(home.join("state.db")).unwrap()
    }

    #[test]
    fn flush_then_recover_roundtrip() {
        let home = temp_home("rt");
        let db = open_db(&home);
        db.ensure_session("sid-1", "cli", None, None, None).unwrap();
        // A pending value carrying its real session_id recovers.
        let pending = vec![(
            "agent:main:telegram:dm:x".to_string(),
            json!({"text": "unsent question", "session_id": "sid-1"}),
        )];
        assert_eq!(flush_pending_to_file(&pending, "shutdown", Some(&home)), 1);
        // A flush file now exists.
        let n_files = std::fs::read_dir(flush_dir(Some(&home))).unwrap().count();
        assert_eq!(n_files, 1);

        let recovered = recover_pending_to_db(&db, Some(&home));
        assert_eq!(recovered, 1);
        assert_eq!(db.message_count("sid-1").unwrap(), 1);
        // The file was consumed.
        assert_eq!(
            std::fs::read_dir(flush_dir(Some(&home))).unwrap().count(),
            0
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn pending_without_session_id_is_preserved() {
        let home = temp_home("preserve");
        let db = open_db(&home);
        let pending = vec![("key".to_string(), json!("orphan text"))]; // string -> {text}, no session_id
        flush_pending_to_file(&pending, "shutdown", Some(&home));
        let recovered = recover_pending_to_db(&db, Some(&home));
        assert_eq!(recovered, 0);
        // File preserved for the next attempt.
        assert_eq!(
            std::fs::read_dir(flush_dir(Some(&home))).unwrap().count(),
            1
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn transcript_cap_drop_replays() {
        let home = temp_home("spool");
        let db = open_db(&home);
        let dir = ensure_flush_dir(Some(&home)).unwrap();
        let payload = json!({
            "reason": TRANSCRIPT_CAP_DROP_REASON,
            "ts": 1000,
            "data": {"session_id": "sid-2", "message": {"role": "assistant", "content": "dropped reply"}}
        });
        write_payload(&dir, &payload).unwrap();
        assert_eq!(recover_pending_to_db(&db, Some(&home)), 1);
        assert_eq!(db.message_count("sid-2").unwrap(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn agent_history_snapshot_is_skipped_by_recover() {
        let home = temp_home("agenthist");
        let db = open_db(&home);
        flush_agent_history_to_file(
            Some("sid-3"),
            &[json!({"role": "user", "content": "hi"})],
            Some(&home),
        );
        // Recovery leaves it for manual salvage (returns 0, file kept).
        assert_eq!(recover_pending_to_db(&db, Some(&home)), 0);
        assert_eq!(
            std::fs::read_dir(flush_dir(Some(&home))).unwrap().count(),
            1
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
