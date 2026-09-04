//! Port of gateway/rich_sent_store.py.
//!
// Public API is ahead of its callers (the Telegram rich-send path wires it).
#![allow(dead_code)]
//!
//! Local index of text we sent via a rich message. Telegram does not echo a
//! rich message's content back in `reply_to_message`, so a reply to one arrives
//! with no quotable text. This remembers `(chat_id, message_id) -> text` at send
//! time so an inbound reply can look up what was referenced. Best-effort and
//! dependency-free: every operation swallows errors and degrades to a no-op /
//! `None` so it can never break a send or an inbound message.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const MAX_ENTRIES: usize = 1000;
const MAX_TEXT_CHARS: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Entry {
    /// The sent text.
    t: String,
    /// Unix seconds when recorded (for oldest-first trimming).
    #[serde(default)]
    ts: i64,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn key(chat_id: &str, message_id: &str) -> String {
    format!("{chat_id}:{message_id}")
}

/// Take a `char`-safe prefix of at most `max` chars (Python `text[:n]` is
/// char-based, not byte-based).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// A best-effort JSON index of sent rich-message text.
pub struct RichSentStore {
    inner: Mutex<HashMap<String, Entry>>,
    path: PathBuf,
}

impl RichSentStore {
    /// Open at `$HERMES_HOME/state/rich_sent_index.json`.
    pub fn open_default() -> Self {
        let path = crate::config_file::hermes_home()
            .join("state")
            .join("rich_sent_index.json");
        Self::open(path)
    }

    pub fn open(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(load(&path)),
            path,
        }
    }

    /// Persist `text` for `(chat_id, message_id)`. No-op on empty text.
    pub fn record(&self, chat_id: &str, message_id: &str, text: &str) {
        if text.is_empty() || chat_id.is_empty() || message_id.is_empty() {
            return;
        }
        let mut map = self.inner.lock().unwrap();
        map.insert(
            key(chat_id, message_id),
            Entry {
                t: truncate_chars(text, MAX_TEXT_CHARS),
                ts: now_secs(),
            },
        );
        // Trim oldest by timestamp when over the cap.
        if map.len() > MAX_ENTRIES {
            let overflow = map.len() - MAX_ENTRIES;
            let mut by_ts: Vec<(String, i64)> =
                map.iter().map(|(k, e)| (k.clone(), e.ts)).collect();
            by_ts.sort_by_key(|(_, ts)| *ts);
            for (k, _) in by_ts.into_iter().take(overflow) {
                map.remove(&k);
            }
        }
        flush(&self.path, &map);
    }

    /// Return the stored text for `(chat_id, message_id)`, or `None`.
    pub fn lookup(&self, chat_id: &str, message_id: &str) -> Option<String> {
        if chat_id.is_empty() || message_id.is_empty() {
            return None;
        }
        self.inner
            .lock()
            .unwrap()
            .get(&key(chat_id, message_id))
            .map(|e| e.t.clone())
            .filter(|t| !t.is_empty())
    }
}

fn load(path: &PathBuf) -> HashMap<String, Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn flush(path: &PathBuf, map: &HashMap<String, Entry>) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let Ok(json) = serde_json::to_string(map) else {
        return;
    };
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_rich_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        p.push("state");
        p.push("rich_sent_index.json");
        p
    }

    #[test]
    fn record_and_lookup() {
        let path = temp_path("rt");
        let store = RichSentStore::open(path.clone());
        assert_eq!(store.lookup("c1", "7"), None);
        store.record("c1", "7", "the briefing text");
        assert_eq!(
            store.lookup("c1", "7").as_deref(),
            Some("the briefing text")
        );
        // A different key is independent.
        assert_eq!(store.lookup("c1", "8"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn empty_inputs_are_noops() {
        let path = temp_path("empty");
        let store = RichSentStore::open(path.clone());
        store.record("c1", "7", ""); // empty text ignored
        store.record("", "7", "x");
        store.record("c1", "", "x");
        assert!(store.lookup("c1", "7").is_none());
        assert!(store.lookup("", "").is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn persists_across_reopen() {
        let path = temp_path("persist");
        {
            RichSentStore::open(path.clone()).record("c", "1", "remember me");
        }
        let store2 = RichSentStore::open(path.clone());
        assert_eq!(store2.lookup("c", "1").as_deref(), Some("remember me"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn trims_oldest_over_cap() {
        let path = temp_path("cap");
        let store = RichSentStore::open(path.clone());
        // Insert MAX_ENTRIES + 5, each with an increasing ts by construction of
        // now_secs may collide, so drive ts explicitly through the map.
        {
            let mut map = store.inner.lock().unwrap();
            for i in 0..(MAX_ENTRIES + 5) {
                map.insert(
                    format!("c:{i}"),
                    Entry {
                        t: format!("m{i}"),
                        ts: i as i64,
                    },
                );
            }
        }
        // A record() call triggers the trim down to the cap.
        store.record("c", "new", "newest");
        let len = store.inner.lock().unwrap().len();
        assert_eq!(len, MAX_ENTRIES);
        // The very oldest (ts 0) was evicted; the newest survives.
        assert!(store.lookup("c", "0").is_none());
        assert_eq!(store.lookup("c", "new").as_deref(), Some("newest"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
