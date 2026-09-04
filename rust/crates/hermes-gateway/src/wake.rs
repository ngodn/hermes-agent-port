//! Partial port of gateway/wake.py: the stateless-delivery persistence path.
//!
// Public API is ahead of its callers (the background-completion path wires it).
#![allow(dead_code)]
//!
//! When a background job completes on a stateless (API-server) session, the
//! client owns the turn after `event.complete` (#85957), so a completion must
//! never be self-POSTed as a new user prompt: that would start an unauthorized
//! agent turn that can blow through a pending human-confirmation gate. Instead
//! the completion is appended to the session transcript as a durable delivery
//! row (a `user`-role message with `display_kind="async_delegation_complete"`
//! and display metadata, the same bookkeeping shape the TUI/desktop pollers
//! use), without running any agent turn. Clients polling the session's messages
//! endpoint then see it immediately, and the next real turn carries it as
//! context.
//!
//! The push-capable and API-server self-POST wake paths in `wake.py` are
//! coupled to the platform-adapter base (`MessageEvent`/`handle_message`) and
//! the API-server adapter, so they land with the adapter subsystem. The
//! delegation-persistence path only needs the session store and is ported here.

use serde_json::{json, Map, Value};

use crate::session_db::{AppendOptions, SessionDb};

/// Display-only metadata for a persisted delegation-delivery row. Mirrors the
/// TUI's `display_kind` consumer contract (task/completed/failed counts and an
/// optional duration) without importing the TUI stack.
pub fn delegation_display_metadata(evt: &Value) -> Value {
    let results: Vec<&Value> = evt
        .get("results")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|r| r.is_object()).collect())
        .unwrap_or_default();

    let task_count = results.len().max(1);
    let completed_count = results
        .iter()
        .filter(|r| {
            matches!(
                r.get("status").and_then(Value::as_str),
                Some("completed") | Some("success")
            )
        })
        .count();
    let failed_count = results
        .iter()
        .filter(|r| {
            matches!(
                r.get("status").and_then(Value::as_str),
                Some("failed") | Some("error")
            )
        })
        .count();

    let mut metadata = Map::new();
    metadata.insert(
        "delegation_id".into(),
        json!(evt
            .get("delegation_id")
            .and_then(Value::as_str)
            .unwrap_or("")),
    );
    metadata.insert("task_count".into(), json!(task_count));
    // Python: completed_count or (task_count - failed_count).
    let completed = if completed_count > 0 {
        completed_count
    } else {
        task_count.saturating_sub(failed_count)
    };
    metadata.insert("completed_count".into(), json!(completed));
    metadata.insert("failed_count".into(), json!(failed_count));

    if let Some(duration) = evt
        .get("total_duration_seconds")
        .or_else(|| evt.get("duration_seconds"))
        .and_then(Value::as_f64)
    {
        metadata.insert("duration_seconds".into(), json!(duration));
    }
    Value::Object(metadata)
}

/// Persist an async-delegation completion as a durable delivery row on the
/// session transcript, WITHOUT running any agent turn. Errors propagate so the
/// caller can release the durable claim and retry instead of losing the event.
pub fn persist_delegation_delivery(
    db: &SessionDb,
    session_id: &str,
    text: &str,
    evt: Option<&Value>,
) -> Result<i64, String> {
    if session_id.is_empty() {
        return Err(
            "persist_delegation_delivery: raw session id required to persist the completion"
                .to_string(),
        );
    }
    let metadata = delegation_display_metadata(evt.unwrap_or(&Value::Null));
    db.append_message_with(
        session_id,
        "user",
        text,
        &AppendOptions {
            display_kind: Some("async_delegation_complete"),
            display_metadata: Some(metadata),
            ..Default::default()
        },
    )
    .map_err(|e| format!("persist_delegation_delivery: append failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> SessionDb {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_wake_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        SessionDb::open(p).unwrap()
    }

    #[test]
    fn metadata_counts_results() {
        let evt = json!({
            "delegation_id": "d1",
            "results": [
                {"status": "completed"},
                {"status": "error"},
                {"status": "success"}
            ],
            "total_duration_seconds": 12.5
        });
        let m = delegation_display_metadata(&evt);
        assert_eq!(m["delegation_id"], json!("d1"));
        assert_eq!(m["task_count"], json!(3));
        assert_eq!(m["completed_count"], json!(2));
        assert_eq!(m["failed_count"], json!(1));
        assert_eq!(m["duration_seconds"], json!(12.5));
    }

    #[test]
    fn metadata_defaults_when_no_results() {
        // No results -> task_count floors at 1, completed = task_count - failed.
        let m = delegation_display_metadata(&json!({}));
        assert_eq!(m["task_count"], json!(1));
        assert_eq!(m["completed_count"], json!(1));
        assert_eq!(m["failed_count"], json!(0));
        assert_eq!(m.get("duration_seconds"), None);
    }

    #[test]
    fn persist_writes_delivery_row() {
        let db = temp_db();
        db.ensure_session("api:s1", "api_server", None, None, None)
            .unwrap();
        let evt = json!({"delegation_id": "d1", "results": [{"status": "completed"}]});
        let id =
            persist_delegation_delivery(&db, "api:s1", "your report is ready", Some(&evt)).unwrap();
        let row = db.get_message(id).unwrap().unwrap();
        assert_eq!(row.role, "user");
        assert_eq!(row.content, "your report is ready");
        assert_eq!(
            row.display_kind.as_deref(),
            Some("async_delegation_complete")
        );
        let meta: Value = serde_json::from_str(row.display_metadata.as_deref().unwrap()).unwrap();
        assert_eq!(meta["delegation_id"], json!("d1"));
    }

    #[test]
    fn persist_requires_session_id() {
        let db = temp_db();
        assert!(persist_delegation_delivery(&db, "", "x", None).is_err());
    }
}
