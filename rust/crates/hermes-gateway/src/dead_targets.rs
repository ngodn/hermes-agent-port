//! Persistent registry of delivery targets confirmed unreachable.
//!
// mark_dead is wired once adapters classify send errors; is_dead/clear are used
// by the Dispatcher now.
#![allow(dead_code)]
//!
//! Port of `gateway/dead_targets.py`. When a platform reports a target chat is
//! permanently gone (deleted group, bot blocked, deactivated user), re-sending
//! on every fan-out wastes the platform's flood budget and spams logs. This
//! registry lets delivery short-circuit a proven-dead target while staying
//! self-healing: any successful send clears the flag.
//!
//! The store is a small JSON file under `$HERMES_HOME/gateway/dead_targets.json`
//! (shared with the Python gateway during the strangler migration). Reads and
//! writes are best-effort: a corrupt or unwritable file degrades to an
//! in-memory-only registry rather than breaking the delivery path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Error kinds that mean the whole chat is unreachable (not transient or
/// thread-level). Mirrors the Python `_DEAD_ERROR_KINDS`.
const DEAD_ERROR_KINDS: [&str; 2] = ["forbidden", "not_found"];

/// One dead-target record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeadEntry {
    pub platform: String,
    pub chat_id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub marked_at: f64,
}

/// Canonical key for a (platform, chat_id) pair.
fn normalize(platform: &str, chat_id: &str) -> String {
    format!("{}:{}", platform.trim().to_lowercase(), chat_id.trim())
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Thread-safe, persistent set of confirmed-dead delivery targets, keyed on
/// `platform:chat_id`.
pub struct DeadTargetRegistry {
    inner: Mutex<HashMap<String, DeadEntry>>,
    path: PathBuf,
}

impl DeadTargetRegistry {
    /// Open the registry at `path`, loading any existing well-shaped entries.
    pub fn new(path: PathBuf) -> Self {
        let inner = Mutex::new(load(&path));
        Self { inner, path }
    }

    /// True when `error_kind` denotes a permanent whole-chat death.
    pub fn is_dead_error_kind(error_kind: Option<&str>) -> bool {
        matches!(error_kind, Some(k) if DEAD_ERROR_KINDS.contains(&k))
    }

    pub fn is_dead(&self, platform: &str, chat_id: &str) -> bool {
        if chat_id.is_empty() {
            return false;
        }
        self.inner
            .lock()
            .unwrap()
            .contains_key(&normalize(platform, chat_id))
    }

    /// Record a target as confirmed-dead. Returns true if newly added.
    pub fn mark_dead(&self, platform: &str, chat_id: &str, reason: &str) -> bool {
        if chat_id.is_empty() {
            return false;
        }
        let key = normalize(platform, chat_id);
        let newly = {
            let mut map = self.inner.lock().unwrap();
            let existed = map.contains_key(&key);
            let mut reason = reason.to_string();
            reason.truncate(200);
            map.insert(
                key.clone(),
                DeadEntry {
                    platform: platform.trim().to_lowercase(),
                    chat_id: chat_id.to_string(),
                    reason,
                    marked_at: now_secs(),
                },
            );
            flush(&self.path, &map);
            !existed
        };
        if newly {
            info!(target = %key, "dead_targets: marked unreachable; future deliveries skipped until a send succeeds");
        }
        newly
    }

    /// Remove a target's dead flag (self-healing). Returns true if it was set.
    pub fn clear(&self, platform: &str, chat_id: &str) -> bool {
        if chat_id.is_empty() {
            return false;
        }
        let key = normalize(platform, chat_id);
        let mut map = self.inner.lock().unwrap();
        if map.remove(&key).is_some() {
            flush(&self.path, &map);
            info!(target = %key, "dead_targets: cleared (delivery succeeded again)");
            true
        } else {
            false
        }
    }

    /// Snapshot of the current dead set.
    pub fn all_dead(&self) -> HashMap<String, DeadEntry> {
        self.inner.lock().unwrap().clone()
    }
}

fn load(path: &PathBuf) -> HashMap<String, DeadEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    // Only keep well-shaped entries; a malformed file degrades to empty.
    match serde_json::from_str::<HashMap<String, DeadEntry>>(&text) {
        Ok(map) => map,
        Err(exc) => {
            debug!(%exc, "dead_targets: could not load; starting empty");
            HashMap::new()
        }
    }
}

fn flush(path: &PathBuf, map: &HashMap<String, DeadEntry>) {
    // Best-effort atomic write: temp file then rename. Never break delivery.
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    let Ok(json) = serde_json::to_string_pretty(map) else {
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
            "hermes_dead_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        p.push("gateway");
        p.push("dead_targets.json");
        p
    }

    #[test]
    fn dead_error_kinds() {
        assert!(DeadTargetRegistry::is_dead_error_kind(Some("forbidden")));
        assert!(DeadTargetRegistry::is_dead_error_kind(Some("not_found")));
        assert!(!DeadTargetRegistry::is_dead_error_kind(Some("timeout")));
        assert!(!DeadTargetRegistry::is_dead_error_kind(None));
    }

    #[test]
    fn mark_is_dead_and_clear_roundtrip() {
        let path = temp_path("rt");
        let reg = DeadTargetRegistry::new(path.clone());
        assert!(!reg.is_dead("telegram", "123"));
        assert!(reg.mark_dead("Telegram", "123", "blocked")); // newly added
        assert!(!reg.mark_dead("telegram", "123", "blocked")); // already present
                                                               // Platform is normalized to lowercase, so casing does not matter.
        assert!(reg.is_dead("telegram", "123"));
        assert!(reg.clear("telegram", "123"));
        assert!(!reg.clear("telegram", "123")); // already cleared
        assert!(!reg.is_dead("telegram", "123"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn persists_across_reopen() {
        let path = temp_path("persist");
        {
            let reg = DeadTargetRegistry::new(path.clone());
            reg.mark_dead("discord", "chan9", "kicked");
        }
        // A fresh registry loads the flushed entry.
        let reg2 = DeadTargetRegistry::new(path.clone());
        assert!(reg2.is_dead("discord", "chan9"));
        let all = reg2.all_dead();
        assert_eq!(all["discord:chan9"].reason, "kicked");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn empty_chat_id_is_never_dead() {
        let reg = DeadTargetRegistry::new(temp_path("empty"));
        assert!(!reg.is_dead("telegram", ""));
        assert!(!reg.mark_dead("telegram", "", "x"));
        assert!(!reg.clear("telegram", ""));
    }
}
