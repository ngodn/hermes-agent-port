//! Port of the session-identity core of gateway/session.py.
//!
// Public API is ahead of some callers while the runner/adapters are ported.
#![allow(dead_code)]
//!
//! `SessionSource` (where a message originated) and `build_session_key` (the
//! single source of truth for deterministic session-key construction), plus the
//! path/key safety guards, id hashing, and `sanitize_model_override`. This is
//! the identity model authz, wake, dispatch, and mirror all key off.
//!
//! The huge `SessionStore` transcript layer (persistence, prompt building,
//! auto-continue) overlaps `session_db.rs` and lands with the agent-core turn
//! path; this is only the identity/key core.
//!
//! `platform` is carried as its wire string value (e.g. `slack`, `whatsapp`,
//! `telegram`, `discord`, `local`) — the gateway's `Platform` enum is richer
//! than `hermes_core::Platform`, and `to_dict`/`build_session_key` work off the
//! string form, so this stays decoupled and faithful to the wire.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::whatsapp_identity::canonical_whatsapp_identifier;

/// Deterministic 12-char hex hash of an identifier.
pub fn hash_id(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let hex = format!("{digest:x}");
    hex[..12].to_string()
}

/// Hash a sender id to `user_<12hex>`.
pub fn hash_sender_id(value: &str) -> String {
    format!("user_{}", hash_id(value))
}

/// Hash the numeric portion of a chat id, preserving a `platform:` prefix.
pub fn hash_chat_id(value: &str) -> String {
    match value.find(':') {
        Some(colon) if colon > 0 => {
            format!("{}:{}", &value[..colon], hash_id(&value[colon + 1..]))
        }
        _ => hash_id(value),
    }
}

/// True if `value` could traverse outside the sessions dir (strict: any `..`,
/// path separator, or leading Windows drive letter). For values that become
/// filesystem paths (session_id).
pub fn is_path_unsafe(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.contains("..") || value.contains('/') || value.contains('\\') {
        return true;
    }
    has_drive_prefix(value)
}

/// True if `value` is a real traversal vector in a session_key (relaxed: `..`,
/// a *leading* separator, or a leading drive letter — interior `/` is allowed,
/// since a logical routing key never touches the filesystem).
pub fn is_session_key_unsafe(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.contains("..") || value.starts_with('/') || value.starts_with('\\') {
        return true;
    }
    has_drive_prefix(value)
}

fn has_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Where a message originated. `platform` is the wire string value. Optional
/// fields default to `None`/false; build with `SessionSource { platform, chat_id,
/// ..Default::default() }`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSource {
    pub platform: String,
    pub chat_id: String,
    pub chat_name: Option<String>,
    /// "dm" | "group" | "channel" | "thread". Empty defaults to "dm".
    pub chat_type: String,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub thread_id: Option<String>,
    pub chat_topic: Option<String>,
    pub user_id_alt: Option<String>,
    pub chat_id_alt: Option<String>,
    pub is_bot: bool,
    pub scope_id: Option<String>,
    /// @deprecated legacy alias for scope_id (D-Q2.5).
    pub guild_id: Option<String>,
    pub parent_chat_id: Option<String>,
    pub message_id: Option<String>,
    pub role_authorized: bool,
    pub profile: Option<String>,
    pub prospective_thread_id: Option<String>,
    pub auto_thread_created: bool,
    pub auto_thread_initial_name: Option<String>,
    pub delivered_via_upstream_relay: bool,
    pub profile_route_rejected: bool,
}

impl SessionSource {
    /// A source with only the required fields set (chat_type defaults to "dm").
    pub fn new(platform: impl Into<String>, chat_id: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
            chat_id: chat_id.into(),
            chat_type: "dm".to_string(),
            ..Default::default()
        }
    }

    fn chat_type_or_dm(&self) -> &str {
        if self.chat_type.is_empty() {
            "dm"
        } else {
            &self.chat_type
        }
    }

    /// Canonical scope (scope_id wins, else the deprecated guild_id alias).
    pub fn scope(&self) -> Option<&str> {
        self.scope_id.as_deref().or(self.guild_id.as_deref())
    }

    /// Human-readable description of the source.
    pub fn description(&self) -> String {
        if self.platform == "local" {
            return "CLI terminal".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        match self.chat_type_or_dm() {
            "dm" => {
                let who = self
                    .user_name
                    .as_deref()
                    .or(self.user_id.as_deref())
                    .unwrap_or("user");
                parts.push(format!("DM with {who}"));
            }
            "group" => parts.push(format!(
                "group: {}",
                self.chat_name.as_deref().unwrap_or(&self.chat_id)
            )),
            "channel" => parts.push(format!(
                "channel: {}",
                self.chat_name.as_deref().unwrap_or(&self.chat_id)
            )),
            _ => parts.push(
                self.chat_name
                    .clone()
                    .unwrap_or_else(|| self.chat_id.clone()),
            ),
        }
        if let Some(tid) = &self.thread_id {
            parts.push(format!("thread: {tid}"));
        }
        parts.join(", ")
    }

    /// Wire form (mirrors Python `to_dict`: always-present core fields, plus the
    /// truthy-only optionals, dual-writing scope as both `scope_id`/`guild_id`).
    pub fn to_dict(&self) -> Value {
        let mut d = Map::new();
        d.insert("platform".into(), json!(self.platform));
        d.insert("chat_id".into(), json!(self.chat_id));
        d.insert("chat_name".into(), opt(&self.chat_name));
        d.insert("chat_type".into(), json!(self.chat_type_or_dm()));
        d.insert("user_id".into(), opt(&self.user_id));
        d.insert("user_name".into(), opt(&self.user_name));
        d.insert("thread_id".into(), opt(&self.thread_id));
        d.insert("chat_topic".into(), opt(&self.chat_topic));
        insert_if(&mut d, "user_id_alt", &self.user_id_alt);
        insert_if(&mut d, "chat_id_alt", &self.chat_id_alt);
        if let Some(scope) = self.scope() {
            if !scope.is_empty() {
                d.insert("scope_id".into(), json!(scope));
                d.insert("guild_id".into(), json!(scope));
            }
        }
        insert_if(&mut d, "parent_chat_id", &self.parent_chat_id);
        insert_if(&mut d, "message_id", &self.message_id);
        insert_if(&mut d, "profile", &self.profile);
        if self.auto_thread_created {
            d.insert("auto_thread_created".into(), json!(true));
        }
        insert_if(
            &mut d,
            "auto_thread_initial_name",
            &self.auto_thread_initial_name,
        );
        insert_if(&mut d, "prospective_thread_id", &self.prospective_thread_id);
        Value::Object(d)
    }

    /// Parse from the wire form (dual-reads scope_id then the guild_id alias).
    pub fn from_dict(data: &Value) -> Self {
        let s = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_string);
        Self {
            platform: s("platform").unwrap_or_default(),
            chat_id: data
                .get("chat_id")
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default(),
            chat_name: s("chat_name"),
            chat_type: s("chat_type").unwrap_or_else(|| "dm".to_string()),
            user_id: s("user_id"),
            user_name: s("user_name"),
            thread_id: s("thread_id"),
            chat_topic: s("chat_topic"),
            user_id_alt: s("user_id_alt"),
            chat_id_alt: s("chat_id_alt"),
            scope_id: s("scope_id").or_else(|| s("guild_id")),
            parent_chat_id: s("parent_chat_id"),
            message_id: s("message_id"),
            profile: s("profile"),
            auto_thread_created: data
                .get("auto_thread_created")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            auto_thread_initial_name: s("auto_thread_initial_name"),
            prospective_thread_id: s("prospective_thread_id"),
            ..Default::default()
        }
    }
}

fn opt(v: &Option<String>) -> Value {
    match v {
        Some(s) => json!(s),
        None => Value::Null,
    }
}

fn insert_if(map: &mut Map<String, Value>, key: &str, v: &Option<String>) {
    if let Some(s) = v {
        if !s.is_empty() {
            map.insert(key.into(), json!(s));
        }
    }
}

/// True when a non-DM session is shared across participants (mirrors the
/// isolation rules in [`build_session_key`]).
pub fn is_shared_multi_user_session(
    source: &SessionSource,
    group_sessions_per_user: bool,
    thread_sessions_per_user: bool,
) -> bool {
    if source.chat_type_or_dm() == "dm" {
        return false;
    }
    if source.thread_id.is_some() {
        return !thread_sessions_per_user;
    }
    !group_sessions_per_user
}

/// The `agent:<ns>` namespace prefix. Default/None/"default" -> `agent:main`
/// (byte-identical to every legacy key); a named profile -> `agent:<profile>`.
fn session_key_namespace(profile: Option<&str>) -> String {
    match profile {
        None => "agent:main".to_string(),
        Some(p) if p.is_empty() || p == "default" => "agent:main".to_string(),
        Some(p) => format!("agent:{p}"),
    }
}

fn canonical_wa(id: &str) -> String {
    let c = canonical_whatsapp_identifier(id);
    if c.is_empty() {
        id.to_string()
    } else {
        c
    }
}

/// Build a deterministic session key from a message source. The single source
/// of truth for session-key construction (see the Python for the full rules).
pub fn build_session_key(
    source: &SessionSource,
    group_sessions_per_user: bool,
    thread_sessions_per_user: bool,
    profile: Option<&str>,
) -> String {
    let ns = session_key_namespace(profile);
    let platform = source.platform.as_str();
    let slack_scope_id: Option<String> = if platform == "slack" {
        source.scope().filter(|s| !s.is_empty()).map(str::to_string)
    } else {
        None
    };

    if source.chat_type_or_dm() == "dm" {
        let dm_chat_id = if platform == "whatsapp" {
            canonical_wa(&source.chat_id)
        } else {
            source.chat_id.clone()
        };
        let mut parts: Vec<String> = vec![ns.clone(), platform.to_string(), "dm".to_string()];
        if let Some(scope) = &slack_scope_id {
            parts.push(scope.clone());
        }
        if !dm_chat_id.is_empty() {
            parts.push(dm_chat_id);
            if let Some(tid) = &source.thread_id {
                parts.push(tid.clone());
            }
            return parts.join(":");
        }
        // No chat_id: fall back to the sender's own identifier before the bare
        // per-platform sink (keeps DMs isolated per user).
        let mut participant = source
            .user_id_alt
            .clone()
            .or_else(|| source.user_id.clone());
        if let Some(p) = &participant {
            if platform == "whatsapp" {
                participant = Some(canonical_wa(p));
            }
        }
        if let Some(p) = participant.filter(|p| !p.is_empty()) {
            parts.push(p);
            if let Some(tid) = &source.thread_id {
                parts.push(tid.clone());
            }
            return parts.join(":");
        }
        if let Some(tid) = &source.thread_id {
            parts.push(tid.clone());
        }
        return parts.join(":");
    }

    // Group / channel / thread.
    let mut participant_id = source
        .user_id_alt
        .clone()
        .or_else(|| source.user_id.clone());
    if let Some(p) = &participant_id {
        if platform == "whatsapp" {
            participant_id = Some(canonical_wa(p));
        }
    }

    // Discord auto-thread continuity: key on the prospective thread id and
    // normalize the chat_type slot to "thread" so the initiating channel
    // message and the later in-thread follow-ups share one session.
    let effective_thread_id = source
        .thread_id
        .clone()
        .or_else(|| source.prospective_thread_id.clone());
    let mut chat_type_slot = source.chat_type_or_dm().to_string();
    if source.prospective_thread_id.is_some() && source.thread_id.is_none() {
        chat_type_slot = "thread".to_string();
    }

    let mut parts: Vec<String> = vec![ns, platform.to_string(), chat_type_slot];
    if let Some(scope) = &slack_scope_id {
        parts.push(scope.clone());
    }
    if !source.chat_id.is_empty() {
        parts.push(source.chat_id.clone());
    }
    if let Some(tid) = &effective_thread_id {
        parts.push(tid.clone());
    }

    // Threads default to shared sessions; per-user isolation only when there is
    // no thread (a regular group) or thread_sessions_per_user is enabled.
    let mut isolate_user = group_sessions_per_user;
    if effective_thread_id.is_some() && !thread_sessions_per_user {
        isolate_user = false;
    }
    if isolate_user {
        if let Some(p) = participant_id.filter(|p| !p.is_empty()) {
            parts.push(p);
        }
    }
    parts.join(":")
}

const PERSISTABLE_MODEL_OVERRIDE_KEYS: [&str; 3] = ["model", "provider", "base_url"];

/// Keep only persistable, non-secret model-override keys as strings. `None`
/// when the input is not an object or nothing persistable remains.
pub fn sanitize_model_override(over: Option<&Value>) -> Option<Value> {
    let obj = over?.as_object()?;
    let mut cleaned = Map::new();
    for key in PERSISTABLE_MODEL_OVERRIDE_KEYS {
        if let Some(v) = obj.get(key) {
            let s = match v {
                Value::Null => continue,
                Value::String(s) if s.is_empty() => continue,
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            cleaned.insert(key.to_string(), json!(s));
        }
    }
    if cleaned.is_empty() {
        None
    } else {
        Some(Value::Object(cleaned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_and_safety() {
        assert_eq!(hash_id("abc").len(), 12);
        assert!(hash_sender_id("u1").starts_with("user_"));
        assert_eq!(
            hash_chat_id("telegram:12345").split(':').next(),
            Some("telegram")
        );
        assert!(is_path_unsafe("../etc"));
        assert!(is_path_unsafe("a/b"));
        assert!(is_path_unsafe("C:evil"));
        assert!(!is_path_unsafe("agent:main:telegram:dm:1"));
        // session_key guard allows interior '/', blocks traversal.
        assert!(!is_session_key_unsafe(
            "agent:main:google_chat:group:spaces/abc"
        ));
        assert!(is_session_key_unsafe("../x"));
        assert!(is_session_key_unsafe("/abs"));
    }

    #[test]
    fn dm_key_isolates_by_chat_id() {
        let s = SessionSource {
            chat_type: "dm".into(),
            ..SessionSource::new("telegram", "999")
        };
        assert_eq!(
            build_session_key(&s, true, false, None),
            "agent:main:telegram:dm:999"
        );
    }

    #[test]
    fn dm_without_chat_falls_back_to_sender() {
        let s = SessionSource {
            chat_id: String::new(),
            chat_type: "dm".into(),
            user_id: Some("u42".into()),
            ..SessionSource::new("telegram", "")
        };
        assert_eq!(
            build_session_key(&s, true, false, None),
            "agent:main:telegram:dm:u42"
        );
    }

    #[test]
    fn group_isolates_participant_when_enabled() {
        let s = SessionSource {
            chat_type: "group".into(),
            user_id: Some("alice".into()),
            ..SessionSource::new("telegram", "grp1")
        };
        // group_sessions_per_user=true -> participant appended.
        assert_eq!(
            build_session_key(&s, true, false, None),
            "agent:main:telegram:group:grp1:alice"
        );
        // disabled -> shared per chat.
        assert_eq!(
            build_session_key(&s, false, false, None),
            "agent:main:telegram:group:grp1"
        );
    }

    #[test]
    fn thread_is_shared_by_default() {
        let s = SessionSource {
            chat_type: "thread".into(),
            thread_id: Some("t7".into()),
            user_id: Some("alice".into()),
            ..SessionSource::new("telegram", "grp1")
        };
        // Thread shared -> no participant, even with group_sessions_per_user.
        assert_eq!(
            build_session_key(&s, true, false, None),
            "agent:main:telegram:thread:grp1:t7"
        );
        // thread_sessions_per_user -> participant appended.
        assert_eq!(
            build_session_key(&s, true, true, None),
            "agent:main:telegram:thread:grp1:t7:alice"
        );
    }

    #[test]
    fn slack_scope_is_prefixed() {
        let s = SessionSource {
            platform: "slack".into(),
            chat_type: "group".into(),
            scope_id: Some("W1".into()),
            ..SessionSource::new("slack", "C1")
        };
        assert_eq!(
            build_session_key(&s, false, false, None),
            "agent:main:slack:group:W1:C1"
        );
    }

    #[test]
    fn discord_prospective_thread_continuity() {
        // Initiating channel message: no thread_id, carries prospective id.
        let init = SessionSource {
            platform: "discord".into(),
            chat_type: "channel".into(),
            prospective_thread_id: Some("m99".into()),
            ..SessionSource::new("discord", "chan1")
        };
        // Follow-up in the real thread with the same id.
        let follow = SessionSource {
            platform: "discord".into(),
            chat_type: "thread".into(),
            thread_id: Some("m99".into()),
            ..SessionSource::new("discord", "chan1")
        };
        let k1 = build_session_key(&init, true, false, None);
        let k2 = build_session_key(&follow, true, false, None);
        assert_eq!(
            k1, k2,
            "initiate-in-channel and continue-in-thread share one key"
        );
        assert_eq!(k1, "agent:main:discord:thread:chan1:m99");
    }

    #[test]
    fn profile_namespaces_the_key() {
        let s = SessionSource {
            chat_type: "dm".into(),
            ..SessionSource::new("telegram", "1")
        };
        assert_eq!(
            build_session_key(&s, true, false, Some("coder")),
            "agent:coder:telegram:dm:1"
        );
        assert_eq!(
            build_session_key(&s, true, false, Some("default")),
            "agent:main:telegram:dm:1"
        );
    }

    #[test]
    fn to_from_dict_roundtrip_and_scope_dual() {
        let s = SessionSource {
            platform: "discord".into(),
            chat_type: "channel".into(),
            scope_id: Some("guild9".into()),
            message_id: Some("m1".into()),
            ..SessionSource::new("discord", "c1")
        };
        let d = s.to_dict();
        // Dual-write scope as both keys.
        assert_eq!(d["scope_id"], json!("guild9"));
        assert_eq!(d["guild_id"], json!("guild9"));
        let back = SessionSource::from_dict(&d);
        assert_eq!(back.chat_id, "c1");
        assert_eq!(back.scope_id.as_deref(), Some("guild9"));
        assert_eq!(back.message_id.as_deref(), Some("m1"));
        // A peer sending only the legacy guild_id is dual-read.
        let legacy = json!({"platform": "discord", "chat_id": "c1", "guild_id": "g2"});
        assert_eq!(
            SessionSource::from_dict(&legacy).scope_id.as_deref(),
            Some("g2")
        );
    }

    #[test]
    fn model_override_sanitized() {
        let over =
            json!({"model": "gpt", "provider": "openai", "api_key": "secret", "base_url": ""});
        let cleaned = sanitize_model_override(Some(&over)).unwrap();
        assert_eq!(cleaned["model"], json!("gpt"));
        assert_eq!(cleaned["provider"], json!("openai"));
        assert!(cleaned.get("api_key").is_none(), "secret dropped");
        assert!(cleaned.get("base_url").is_none(), "empty dropped");
        // Nothing persistable -> None.
        assert!(sanitize_model_override(Some(&json!({"api_key": "x"}))).is_none());
        assert!(sanitize_model_override(Some(&json!("nope"))).is_none());
    }

    #[test]
    fn shared_multi_user_rules() {
        let dm = SessionSource {
            chat_type: "dm".into(),
            ..SessionSource::new("t", "1")
        };
        assert!(!is_shared_multi_user_session(&dm, true, false));
        let grp = SessionSource {
            chat_type: "group".into(),
            ..SessionSource::new("t", "1")
        };
        assert!(!is_shared_multi_user_session(&grp, true, false)); // isolated
        assert!(is_shared_multi_user_session(&grp, false, false)); // shared
        let thr = SessionSource {
            chat_type: "thread".into(),
            thread_id: Some("t1".into()),
            ..SessionSource::new("t", "1")
        };
        assert!(is_shared_multi_user_session(&thr, true, false)); // shared by default
        assert!(!is_shared_multi_user_session(&thr, true, true)); // per-user
    }
}
