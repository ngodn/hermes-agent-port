//! Port of gateway/mirror.py.
//!
// Public API is ahead of its callers (send_message / cron delivery wire it).
#![allow(dead_code)]
//!
//! Session mirroring for cross-platform message delivery. When a message is
//! sent to a platform (via send_message or cron delivery), this appends a
//! "delivery-mirror" record to the target session's transcript so the
//! receiving-side agent has context about what was sent.
//!
//! `role` defaults to `assistant` (correct for the interactive send_message
//! mirror, where the mirrored text is the agent's own reply). Callers mirroring
//! text that is NOT the agent speaking (a cron brief delivered out-of-band)
//! must pass `role="user"`: only role+content persist at the SQLite boundary,
//! so on replay an assistant-role mirror is indistinguishable from a real
//! assistant turn and produces assistant->assistant pairs that break strict-
//! alternation providers (#2221); a user-role mirror collapses safely.
//!
//! Never fatal: all failures are swallowed and reported as `false`.

use crate::session_db::{AppendOptions, SessionDb};

/// Append a delivery-mirror message to the target session's transcript.
///
/// When `session_id` is `Some`, it is used directly (the caller holds the
/// precise target, e.g. a cron in_channel seed). Otherwise the session is
/// located by origin (`chat_id` + optional `thread_id`), which refuses to guess
/// on an ambiguous chat rather than contaminate another session. Returns
/// whether the mirror was written.
#[allow(clippy::too_many_arguments)]
pub fn mirror_to_session(
    db: &SessionDb,
    chat_id: &str,
    message_text: &str,
    source_label: &str,
    thread_id: Option<&str>,
    role: &str,
    session_id: Option<&str>,
) -> bool {
    let resolved = match session_id.filter(|s| !s.is_empty()) {
        Some(sid) => Some(sid.to_string()),
        None => db.find_session_by_origin(chat_id, thread_id).ok().flatten(),
    };
    let Some(session_id) = resolved else {
        tracing::warn!(
            %chat_id,
            thread_id = thread_id.unwrap_or(""),
            "mirror: no unambiguous session found (origin-scan bailed)"
        );
        return false;
    };

    // Only role + content persist at the SQLite boundary; the mirror /
    // mirror_source metadata is intentionally dropped (matches Python).
    match db.append_message_with(&session_id, role, message_text, &AppendOptions::default()) {
        Ok(_) => {
            tracing::debug!(%session_id, source = source_label, "mirror: wrote to session");
            true
        }
        Err(e) => {
            tracing::warn!(%session_id, %e, "mirror failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> SessionDb {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_mirror_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        SessionDb::open(p).unwrap()
    }

    #[test]
    fn explicit_session_id_appends() {
        let db = temp_db();
        db.ensure_session("sid-explicit", "cli", None, Some("c1"), None)
            .unwrap();
        assert!(mirror_to_session(
            &db,
            "c1",
            "here is your brief",
            "cron",
            None,
            "user",
            Some("sid-explicit"),
        ));
        assert_eq!(db.message_count("sid-explicit").unwrap(), 1);
    }

    #[test]
    fn origin_scan_finds_unique_session() {
        let db = temp_db();
        db.ensure_session("sid-a", "telegram", None, Some("chatA"), Some("dm"))
            .unwrap();
        // No explicit id -> resolves by chat_id (unique).
        assert!(mirror_to_session(
            &db,
            "chatA",
            "reply",
            "send_message",
            None,
            "assistant",
            None
        ));
        assert_eq!(db.message_count("sid-a").unwrap(), 1);
    }

    #[test]
    fn ambiguous_chat_bails_out() {
        let db = temp_db();
        // Two sessions share one chat_id (flat + a thread session) with no
        // thread_id to disambiguate -> refuse to guess, drop the mirror.
        db.ensure_session("sid-flat", "telegram", None, Some("chatB"), Some("group"))
            .unwrap();
        db.ensure_session("sid-thread", "telegram", None, Some("chatB"), Some("forum"))
            .unwrap();
        assert!(!mirror_to_session(
            &db,
            "chatB",
            "x",
            "send_message",
            None,
            "assistant",
            None
        ));
        assert_eq!(db.message_count("sid-flat").unwrap(), 0);
        assert_eq!(db.message_count("sid-thread").unwrap(), 0);
    }

    #[test]
    fn thread_id_disambiguates() {
        let db = temp_db();
        db.ensure_session("sid-flat", "telegram", None, Some("chatC"), Some("group"))
            .unwrap();
        // A thread session carrying an explicit thread_id.
        db.ensure_session("sid-t1", "telegram", None, Some("chatC"), Some("forum"))
            .unwrap();
        db.set_thread_id("sid-t1", "topic-7").unwrap();
        assert!(mirror_to_session(
            &db,
            "chatC",
            "in topic",
            "send_message",
            Some("topic-7"),
            "assistant",
            None
        ));
        assert_eq!(db.message_count("sid-t1").unwrap(), 1);
    }
}
