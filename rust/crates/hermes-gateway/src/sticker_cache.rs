//! Port of gateway/sticker_cache.py.
//!
// Public API is ahead of its callers (the Telegram sticker path wires it).
#![allow(dead_code)]
//!
//! Sticker description cache for Telegram. Sticker images are described once via
//! the vision tool and cached by `file_unique_id` so the same sticker is not
//! re-analyzed on every send. Also builds the warm-style injection text that
//! tells the agent what a sticker depicts. Best-effort JSON at
//! `$HERMES_HOME/sticker_cache.json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Vision prompt for describing stickers (kept concise to save tokens).
pub const STICKER_VISION_PROMPT: &str =
    "Describe this sticker in 1-2 sentences. Focus on what it depicts -- character, action, emotion. Be concise and objective.";

/// A cached sticker description.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StickerEntry {
    pub description: String,
    #[serde(default)]
    pub emoji: String,
    #[serde(default)]
    pub set_name: String,
    #[serde(default)]
    pub cached_at: f64,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Persistent sticker-description cache keyed by `file_unique_id`.
pub struct StickerCache {
    inner: Mutex<HashMap<String, StickerEntry>>,
    path: PathBuf,
}

impl StickerCache {
    /// Open at `$HERMES_HOME/sticker_cache.json`.
    pub fn open_default() -> Self {
        Self::open(crate::config_file::hermes_home().join("sticker_cache.json"))
    }

    pub fn open(path: PathBuf) -> Self {
        Self {
            inner: Mutex::new(load(&path)),
            path,
        }
    }

    /// Look up a cached sticker description, or `None`.
    pub fn get(&self, file_unique_id: &str) -> Option<StickerEntry> {
        self.inner.lock().unwrap().get(file_unique_id).cloned()
    }

    /// Store a sticker description.
    pub fn put(&self, file_unique_id: &str, description: &str, emoji: &str, set_name: &str) {
        let mut map = self.inner.lock().unwrap();
        map.insert(
            file_unique_id.to_string(),
            StickerEntry {
                description: description.to_string(),
                emoji: emoji.to_string(),
                set_name: set_name.to_string(),
                cached_at: now_secs(),
            },
        );
        flush(&self.path, &map);
    }
}

/// Warm-style injection text for a described sticker, e.g.
/// `[The user sent a sticker 😀 from "MyPack"~ It shows: "A cat waving" (=^.w.^=)]`.
pub fn build_sticker_injection(description: &str, emoji: &str, set_name: &str) -> String {
    let context = if !set_name.is_empty() && !emoji.is_empty() {
        format!(" {emoji} from \"{set_name}\"")
    } else if !emoji.is_empty() {
        format!(" {emoji}")
    } else {
        String::new()
    };
    format!("[The user sent a sticker{context}~ It shows: \"{description}\" (=^.w.^=)]")
}

/// Injection text for animated/video stickers that cannot be analyzed.
pub fn build_animated_sticker_injection(emoji: &str) -> String {
    if !emoji.is_empty() {
        format!(
            "[The user sent an animated sticker {emoji}~ I can't see animated ones yet, but the emoji suggests: {emoji}]"
        )
    } else {
        "[The user sent an animated sticker~ I can't see animated ones yet]".to_string()
    }
}

fn load(path: &PathBuf) -> HashMap<String, StickerEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn flush(path: &PathBuf, map: &HashMap<String, StickerEntry>) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
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
            "hermes_sticker_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        p.push("sticker_cache.json");
        p
    }

    #[test]
    fn put_get_roundtrip_and_persist() {
        let path = temp_path("rt");
        {
            let c = StickerCache::open(path.clone());
            assert!(c.get("fid1").is_none());
            c.put("fid1", "a cat waving", "😀", "MyPack");
            let e = c.get("fid1").unwrap();
            assert_eq!(e.description, "a cat waving");
            assert_eq!(e.emoji, "😀");
            assert_eq!(e.set_name, "MyPack");
            assert!(e.cached_at > 0.0);
        }
        // Reopen: the entry persisted.
        let c2 = StickerCache::open(path.clone());
        assert_eq!(c2.get("fid1").unwrap().description, "a cat waving");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn injection_text_variants() {
        assert_eq!(
            build_sticker_injection("A cat waving", "😀", "MyPack"),
            "[The user sent a sticker 😀 from \"MyPack\"~ It shows: \"A cat waving\" (=^.w.^=)]"
        );
        assert_eq!(
            build_sticker_injection("A dog", "🐶", ""),
            "[The user sent a sticker 🐶~ It shows: \"A dog\" (=^.w.^=)]"
        );
        assert_eq!(
            build_sticker_injection("Something", "", ""),
            "[The user sent a sticker~ It shows: \"Something\" (=^.w.^=)]"
        );
    }

    #[test]
    fn animated_injection_variants() {
        assert_eq!(
            build_animated_sticker_injection("🎉"),
            "[The user sent an animated sticker 🎉~ I can't see animated ones yet, but the emoji suggests: 🎉]"
        );
        assert_eq!(
            build_animated_sticker_injection(""),
            "[The user sent an animated sticker~ I can't see animated ones yet]"
        );
    }
}
