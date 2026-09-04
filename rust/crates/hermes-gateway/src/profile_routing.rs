//! Port of gateway/profile_routing.py.
//!
// Public API is ahead of its callers (the inbound routing path wires it).
#![allow(dead_code)]
//!
//! Profile-based routing with hierarchical matching: a single Hermes instance
//! can route specific Discord guilds/channels/threads (and WhatsApp users) to
//! different profiles, each with its own model, tools, memory, and persona.
//!
//! Matching priority (most specific first): platform+chat+thread (14) >
//! platform+chat (6) > platform+guild (2) > no match (default profile). For
//! Discord threads/forum posts, `parent_chat_id` carries the direct parent, so a
//! channel-keyed route matches both direct messages and any thread/post under
//! that channel. WhatsApp `chat_id` also matches across user-identity forms
//! (bare number, JID, LID) after the exact-string check; groups/broadcasts stay
//! exact-only.

use serde_json::Value;

use crate::profile_name::{normalize_profile_name, validate_profile_name};
use crate::whatsapp_identity::expand_whatsapp_aliases;

// Baileys and Cloud share phone/JID/LID identity rules. Other platforms keep
// exact string compare so Telegram numeric ids and Discord snowflakes are
// unchanged.
const WHATSAPP_IDENTITY_PLATFORMS: [&str; 2] = ["whatsapp", "whatsapp_cloud"];
const WHATSAPP_NON_USER_SUFFIXES: [&str; 3] = ["@g.us", "@broadcast", "@newsletter"];

/// True for group / broadcast / newsletter JIDs (chats, not a sender identity).
fn is_whatsapp_non_user_chat(chat_id: Option<&str>) -> bool {
    let Some(cid) = chat_id else { return false };
    let cid = cid.trim().to_lowercase();
    WHATSAPP_NON_USER_SUFFIXES
        .iter()
        .any(|suffix| cid.ends_with(suffix))
}

/// True when two WhatsApp *user* chat_ids refer to the same person (a bare
/// phone number, an `@s.whatsapp.net` JID, and an `@lid` LID collapse to one
/// identity). Group/broadcast JIDs are excluded; non-WhatsApp platforms return
/// false (exact match only).
fn whatsapp_user_chat_ids_match(platform: &str, left: Option<&str>, right: Option<&str>) -> bool {
    if !WHATSAPP_IDENTITY_PLATFORMS.contains(&platform.trim().to_lowercase().as_str()) {
        return false;
    }
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if is_whatsapp_non_user_chat(Some(left)) || is_whatsapp_non_user_chat(Some(right)) {
        return false;
    }
    let left_aliases = expand_whatsapp_aliases(left);
    if left_aliases.is_empty() {
        return false;
    }
    let right_aliases = expand_whatsapp_aliases(right);
    left_aliases.intersection(&right_aliases).next().is_some()
}

/// A single routing rule mapping a platform scope to a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRoute {
    pub name: String,
    pub platform: String,
    pub profile: String,
    pub guild_id: Option<String>,
    pub chat_id: Option<String>,
    pub thread_id: Option<String>,
    pub enabled: bool,
}

impl ProfileRoute {
    /// Higher value = more specific match (guild 2 + chat 4 + thread 8).
    pub fn specificity(&self) -> i32 {
        let mut s = 0;
        if self.guild_id.as_deref().is_some_and(|v| !v.is_empty()) {
            s += 2;
        }
        if self.chat_id.as_deref().is_some_and(|v| !v.is_empty()) {
            s += 4;
        }
        if self.thread_id.as_deref().is_some_and(|v| !v.is_empty()) {
            s += 8;
        }
        s
    }

    /// True if this route matches the given source fields. All configured
    /// discriminators are matched conjunctively (AND).
    pub fn matches(
        &self,
        platform: &str,
        guild_id: Option<&str>,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
        parent_chat_id: Option<&str>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        if self.platform != platform {
            return false;
        }
        if let Some(rt) = self.thread_id.as_deref().filter(|v| !v.is_empty()) {
            if Some(rt) != thread_id {
                return false;
            }
        }
        if let Some(rc) = self.chat_id.as_deref().filter(|v| !v.is_empty()) {
            let exact = Some(rc) == chat_id || Some(rc) == parent_chat_id;
            let wa_match = whatsapp_user_chat_ids_match(platform, Some(rc), chat_id)
                || whatsapp_user_chat_ids_match(platform, Some(rc), parent_chat_id);
            if !(exact || wa_match) {
                return false;
            }
        }
        if let Some(rg) = self.guild_id.as_deref().filter(|v| !v.is_empty()) {
            if Some(rg) != guild_id {
                return false;
            }
        }
        true
    }
}

/// Normalize a route discriminator to a string for strict equality matching.
///
/// YAML loads unquoted numeric ids (Discord snowflakes, Telegram negative chat
/// ids) as integers, while inbound source fields are always strings, so an
/// integer here must be stringified or `matches()` fails silently. Only
/// integers are the legitimate YAML-numeric case; booleans, floats, and other
/// shapes can never equal an inbound id, so they are passed through stringified
/// with a load-time warning instead of being silently "fixed" (#86470).
fn coerce_route_id(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) if !n.is_f64() => Some(n.to_string()),
        Some(other) => {
            tracing::warn!(
                value = %other,
                "profile route discriminator can never match an inbound id; quote it in config.yaml"
            );
            Some(coarse_stringify(other))
        }
    }
}

/// A best-effort string form for a non-string, non-integer discriminator (only
/// reached for values that can never match anyway).
fn coarse_stringify(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn str_field<'a>(entry: &'a serde_json::Map<String, Value>, key: &str) -> &'a str {
    entry.get(key).and_then(Value::as_str).unwrap_or("")
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None => true, // Python default for `enabled` is True.
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Parse `gateway.profile_routes` into routes sorted by specificity (most
/// specific first, stable within a specificity so config order is a tiebreak).
pub fn parse_profile_routes(raw: Option<&Value>) -> Vec<ProfileRoute> {
    let Some(Value::Array(entries)) = raw else {
        return Vec::new();
    };
    let mut routes: Vec<ProfileRoute> = Vec::new();
    for entry in entries {
        let Value::Object(entry) = entry else {
            continue;
        };
        let name = str_field(entry, "name").to_string();
        let platform = str_field(entry, "platform").to_string();
        let profile = str_field(entry, "profile");
        if platform.is_empty() || profile.is_empty() {
            tracing::warn!(%name, "skipping profile route: missing platform or profile");
            continue;
        }
        // Validate the profile name to prevent path traversal.
        let profile = match normalize_profile_name(profile)
            .and_then(|p| validate_profile_name(&p).map(|_| p))
        {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%name, %err, "skipping profile route: invalid profile name");
                continue;
            }
        };
        routes.push(ProfileRoute {
            name,
            platform,
            profile,
            guild_id: coerce_route_id(entry.get("guild_id")),
            chat_id: coerce_route_id(entry.get("chat_id")),
            thread_id: coerce_route_id(entry.get("thread_id")),
            enabled: truthy(entry.get("enabled")),
        });
    }
    // Most specific first; stable sort keeps config order within a specificity.
    routes.sort_by_key(|r| std::cmp::Reverse(r.specificity()));
    tracing::debug!(
        count = routes.len(),
        "loaded profile routes (most-specific-first)"
    );
    routes
}

/// The best-matching route, or `None` for no match (use the default profile).
pub fn match_profile_route<'a>(
    routes: &'a [ProfileRoute],
    platform: &str,
    guild_id: Option<&str>,
    chat_id: Option<&str>,
    thread_id: Option<&str>,
    parent_chat_id: Option<&str>,
) -> Option<&'a ProfileRoute> {
    routes
        .iter()
        .find(|route| route.matches(platform, guild_id, chat_id, thread_id, parent_chat_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes_json() -> Value {
        serde_json::json!([
            {"name": "guild", "platform": "discord", "guild_id": 111, "profile": "Server"},
            {"name": "channel", "platform": "discord", "chat_id": 222, "profile": "channel-p"},
            {"name": "thread", "platform": "discord", "chat_id": 222, "thread_id": 333, "profile": "thread-p"},
            {"name": "wa", "platform": "whatsapp", "chat_id": "15551234567", "profile": "owner"},
            {"name": "disabled", "platform": "slack", "chat_id": "C1", "profile": "x", "enabled": false}
        ])
    }

    #[test]
    fn parse_coerces_numeric_ids_and_sorts_by_specificity() {
        let routes = parse_profile_routes(Some(&routes_json()));
        // Sorted most-specific first: thread (12) > channel (4) > wa (4) > guild (2) > disabled (4).
        // thread has chat+thread = 12; it must be first.
        assert_eq!(routes[0].name, "thread");
        // Numeric guild id coerced to string.
        let guild = routes.iter().find(|r| r.name == "guild").unwrap();
        assert_eq!(guild.guild_id.as_deref(), Some("111"));
        assert_eq!(guild.specificity(), 2);
    }

    #[test]
    fn missing_platform_or_profile_is_skipped() {
        let raw = serde_json::json!([
            {"name": "no-plat", "profile": "p"},
            {"name": "no-prof", "platform": "discord"}
        ]);
        assert!(parse_profile_routes(Some(&raw)).is_empty());
    }

    #[test]
    fn invalid_profile_name_is_skipped() {
        let raw = serde_json::json!([
            {"name": "bad", "platform": "discord", "chat_id": "1", "profile": "../etc"}
        ]);
        assert!(parse_profile_routes(Some(&raw)).is_empty());
        // A mixed-case name is normalized then accepted.
        let ok = serde_json::json!([
            {"name": "ok", "platform": "discord", "chat_id": "1", "profile": "MyProfile"}
        ]);
        let routes = parse_profile_routes(Some(&ok));
        assert_eq!(routes[0].profile, "myprofile");
    }

    #[test]
    fn thread_route_needs_exact_thread() {
        let routes = parse_profile_routes(Some(&routes_json()));
        // A message in channel 222, thread 333 -> most specific thread route.
        let m = match_profile_route(&routes, "discord", None, Some("222"), Some("333"), None);
        assert_eq!(m.unwrap().name, "thread");
        // Same channel, different thread -> falls back to the channel route.
        let m2 = match_profile_route(&routes, "discord", None, Some("222"), Some("999"), None);
        assert_eq!(m2.unwrap().name, "channel");
    }

    #[test]
    fn channel_route_matches_thread_via_parent() {
        let routes = parse_profile_routes(Some(&routes_json()));
        // A thread whose parent is channel 222, no thread route -> channel match.
        let m = match_profile_route(
            &routes,
            "discord",
            None,
            Some("888"),
            Some("777"),
            Some("222"),
        );
        assert_eq!(m.unwrap().name, "channel");
    }

    #[test]
    fn guild_constraint_requires_guild() {
        let raw = serde_json::json!([
            {"name": "g+c", "platform": "discord", "guild_id": "G", "chat_id": "C", "profile": "p"}
        ]);
        let routes = parse_profile_routes(Some(&raw));
        // chat matches but guild missing -> no match.
        assert!(match_profile_route(&routes, "discord", None, Some("C"), None, None).is_none());
        // both match -> match.
        assert!(
            match_profile_route(&routes, "discord", Some("G"), Some("C"), None, None).is_some()
        );
    }

    #[test]
    fn disabled_route_never_matches() {
        let routes = parse_profile_routes(Some(&routes_json()));
        assert!(match_profile_route(&routes, "slack", None, Some("C1"), None, None).is_none());
    }

    #[test]
    fn no_match_returns_none() {
        let routes = parse_profile_routes(Some(&routes_json()));
        assert!(match_profile_route(&routes, "telegram", None, Some("42"), None, None).is_none());
    }
}
