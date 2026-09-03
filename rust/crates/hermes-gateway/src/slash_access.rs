//! Port of gateway/slash_access.py.
//!
// Public access policy API is ahead of its callers while gateway/run.py is ported.
#![allow(dead_code)]
//!
//! Per-platform slash command access control. Resolves per-platform and per-scope
//! slash command permissions so operators can restrict privileged slash commands to
//! designated admins while preserving guest access to safe read-only commands.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Slash commands that must stay reachable for any allowed user, even when
/// slash gating is enabled and the user has no commands listed.
pub const ALWAYS_ALLOWED_FOR_USERS: &[&str] = &["help", "whoami"];

/// Returns true if the canonical command is in the always-allowed baseline floor.
pub fn is_always_allowed(canonical_cmd: &str) -> bool {
    matches!(canonical_cmd, "help" | "whoami")
}

/// Context scope for slash command access control: direct messages vs multi-user groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Dm,
    Group,
}

impl Scope {
    /// Return the canonical lowercase string representation of this scope.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dm => "dm",
            Self::Group => "group",
        }
    }

    /// Resolve scope from an optional chat type string.
    ///
    /// "dm", "direct", and "private" map to Scope::Dm. All other chat types
    /// (including "group", "channel", "thread", empty strings, or None) map to Scope::Group.
    pub fn from_chat_type(chat_type: Option<&str>) -> Self {
        match chat_type {
            Some(ct) if !ct.trim().is_empty() => {
                let lower = ct.trim().to_ascii_lowercase();
                if matches!(lower.as_str(), "dm" | "direct" | "private") {
                    Self::Dm
                } else {
                    Self::Group
                }
            }
            _ => Self::Group,
        }
    }

    /// Return the (admin_key, user_cmd_key) configuration key names for this scope.
    pub fn config_keys(&self) -> (&'static str, &'static str) {
        match self {
            Self::Group => ("group_allow_admin_from", "group_user_allowed_commands"),
            Self::Dm => ("allow_admin_from", "user_allowed_commands"),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "dm" | "direct" | "private" => Self::Dm,
            _ => Self::Group,
        })
    }
}

/// Return the (admin_key, user_cmd_key) configuration key names for a scope.
pub fn keys_for_scope(scope: Scope) -> (&'static str, &'static str) {
    scope.config_keys()
}

/// Resolve the scope from an optional chat type string.
pub fn scope_for_chat_type(chat_type: Option<&str>) -> Scope {
    Scope::from_chat_type(chat_type)
}

/// Normalize a YAML/JSON-loaded admin or user list into a set of strings.
///
/// Accepts null, arrays, comma-separated strings, numbers, or booleans.
/// Stringifies each entry, strips whitespace, and drops empty entries.
pub fn coerce_id_list(raw: Option<&Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(raw) = raw else {
        return out;
    };
    match raw {
        Value::Null => {}
        Value::Array(arr) => {
            for item in arr {
                match item {
                    Value::String(s) => {
                        let trimmed = s.trim();
                        if !trimmed.is_empty() {
                            out.insert(trimmed.to_string());
                        }
                    }
                    Value::Number(n) => {
                        let s = n.to_string();
                        let trimmed = s.trim();
                        if !trimmed.is_empty() {
                            out.insert(trimmed.to_string());
                        }
                    }
                    Value::Bool(b) => {
                        out.insert(b.to_string());
                    }
                    Value::Null | Value::Array(_) | Value::Object(_) => {}
                }
            }
        }
        Value::String(s) => {
            for part in s.split(',') {
                let trimmed = part.trim();
                if !trimmed.is_empty() {
                    out.insert(trimmed.to_string());
                }
            }
        }
        Value::Number(n) => {
            let s = n.to_string();
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                out.insert(trimmed.to_string());
            }
        }
        Value::Bool(b) => {
            out.insert(b.to_string());
        }
        Value::Object(_) => {}
    }
    out
}

/// Normalize a slash command allowlist into a set of canonical command names.
///
/// Strips leading slashes (allowing both `help` and `/help`), trims whitespace,
/// and canonicalizes to lowercase. Empty entries are dropped.
pub fn coerce_command_list(raw: Option<&Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(raw) = raw else {
        return out;
    };
    match raw {
        Value::Null => {}
        Value::Array(arr) => {
            for item in arr {
                match item {
                    Value::String(s) => {
                        let cleaned = s.trim().trim_start_matches('/').to_ascii_lowercase();
                        if !cleaned.is_empty() {
                            out.insert(cleaned);
                        }
                    }
                    Value::Number(n) => {
                        let s = n.to_string();
                        let cleaned = s.trim().trim_start_matches('/').to_ascii_lowercase();
                        if !cleaned.is_empty() {
                            out.insert(cleaned);
                        }
                    }
                    Value::Bool(b) => {
                        out.insert(b.to_string());
                    }
                    Value::Null | Value::Array(_) | Value::Object(_) => {}
                }
            }
        }
        Value::String(s) => {
            for part in s.split(',') {
                let cleaned = part.trim().trim_start_matches('/').to_ascii_lowercase();
                if !cleaned.is_empty() {
                    out.insert(cleaned);
                }
            }
        }
        Value::Number(n) => {
            let s = n.to_string();
            let cleaned = s.trim().trim_start_matches('/').to_ascii_lowercase();
            if !cleaned.is_empty() {
                out.insert(cleaned);
            }
        }
        Value::Bool(b) => {
            out.insert(b.to_string());
        }
        Value::Object(_) => {}
    }
    out
}

/// Resolved access policy for a single (platform, scope) pair.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SlashAccessPolicy {
    /// Whether slash command gating is active for this scope.
    pub enabled: bool,
    /// User IDs that get access to every registered slash command.
    pub admin_user_ids: HashSet<String>,
    /// Slash command names that non-admin users may execute.
    pub user_allowed_commands: HashSet<String>,
}

impl SlashAccessPolicy {
    /// Create a disabled policy (slash gating inactive, allow all commands).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            admin_user_ids: HashSet::new(),
            user_allowed_commands: HashSet::new(),
        }
    }

    /// Create a new access policy.
    pub fn new(
        enabled: bool,
        admin_user_ids: HashSet<String>,
        user_allowed_commands: HashSet<String>,
    ) -> Self {
        Self {
            enabled,
            admin_user_ids,
            user_allowed_commands,
        }
    }

    /// Check if a user ID has admin privileges under this policy.
    ///
    /// When gating is disabled (`enabled == false`), returns true for all users
    /// so downstream callers can use `is_admin` and `can_run` uniformly.
    /// When gating is enabled, returns true only if `user_id` is present, non-empty,
    /// and listed in `admin_user_ids`.
    ///
    /// Matches the Python exactly: the id is compared as-is (no trimming), so a
    /// value with surrounding whitespace does not match a trimmed admin entry.
    /// Only None and the empty string are the falsy short-circuit.
    pub fn is_admin(&self, user_id: Option<&str>) -> bool {
        if !self.enabled {
            return true;
        }
        match user_id {
            None | Some("") => false,
            Some(uid) => self.admin_user_ids.contains(uid),
        }
    }

    /// Check if a user can execute a specific slash command under this policy.
    ///
    /// `canonical_cmd` must already be canonical (lowercase, no leading slash),
    /// matching the Python contract: this method compares it directly and does
    /// NOT normalize, so `"/help"` is not treated as `"help"` here. The command
    /// is canonicalized upstream at the dispatch site.
    ///
    /// Evaluation order:
    /// 1. If gating is disabled -> allow (true).
    /// 2. If user is an admin -> allow (true).
    /// 3. If command is empty -> deny (false).
    /// 4. If command is in the baseline floor (`help`, `whoami`) -> allow (true).
    /// 5. If command is in `user_allowed_commands` -> allow (true).
    /// 6. Otherwise -> deny (false).
    pub fn can_run(&self, user_id: Option<&str>, canonical_cmd: &str) -> bool {
        if !self.enabled {
            return true;
        }
        if self.is_admin(user_id) {
            return true;
        }
        if canonical_cmd.is_empty() {
            return false;
        }
        if is_always_allowed(canonical_cmd) {
            return true;
        }
        self.user_allowed_commands.contains(canonical_cmd)
    }
}

/// Extract the `extra` dictionary from a platform configuration value.
pub fn platform_extra(platform_config: Option<&Value>) -> Value {
    let Some(config) = platform_config else {
        return Value::Object(serde_json::Map::new());
    };
    if let Value::Object(map) = config {
        if let Some(extra @ Value::Object(_)) = map.get("extra") {
            return extra.clone();
        }
        // If the object itself contains settings directly (e.g. in test fixtures)
        return config.clone();
    }
    Value::Object(serde_json::Map::new())
}

/// Build a policy from a platform's `extra` config map for one scope.
///
/// Direct message (DM) scope falls back to group scope keys only for
/// `user_allowed_commands` when the DM scope did not specify its own list.
/// Admin lists are never cross-scope: an admin in DMs is not implicitly an admin
/// in a group.
pub fn policy_from_extra(extra: &Value, scope: Scope) -> SlashAccessPolicy {
    let (admin_key, cmd_key) = scope.config_keys();
    let map = extra.as_object();

    let admin_ids = coerce_id_list(map.and_then(|m| m.get(admin_key)));
    let mut cmds = coerce_command_list(map.and_then(|m| m.get(cmd_key)));

    if scope == Scope::Dm && cmds.is_empty() {
        // DM did not specify commands; let group's user_allowed_commands fall through.
        cmds = coerce_command_list(map.and_then(|m| m.get("group_user_allowed_commands")));
    }

    let enabled = !admin_ids.is_empty();
    SlashAccessPolicy {
        enabled,
        admin_user_ids: admin_ids,
        user_allowed_commands: cmds,
    }
}

/// Describes where a message originated from (platform, chat, user, chat type).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionSource {
    pub platform: Option<String>,
    pub chat_id: Option<String>,
    pub chat_name: Option<String>,
    pub chat_type: Option<String>,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
}

impl SessionSource {
    /// Create a new session source with minimal required fields.
    pub fn new(
        platform: impl Into<String>,
        chat_id: impl Into<String>,
        chat_type: impl Into<String>,
        user_id: Option<impl Into<String>>,
    ) -> Self {
        Self {
            platform: Some(platform.into()),
            chat_id: Some(chat_id.into()),
            chat_name: None,
            chat_type: Some(chat_type.into()),
            user_id: user_id.map(Into::into),
            user_name: None,
        }
    }

    /// Create a session source from a typed [`hermes_core::Platform`].
    pub fn from_platform(
        platform: hermes_core::Platform,
        chat_id: impl Into<String>,
        chat_type: impl Into<String>,
        user_id: Option<impl Into<String>>,
    ) -> Self {
        let platform_name = match platform {
            hermes_core::Platform::Cli => "cli",
            hermes_core::Platform::Telegram => "telegram",
            hermes_core::Platform::Discord => "discord",
            hermes_core::Platform::Slack => "slack",
            hermes_core::Platform::WhatsApp => "whatsapp",
            hermes_core::Platform::Signal => "signal",
        };
        Self::new(platform_name, chat_id, chat_type, user_id)
    }
}

/// Resolve the access policy for a session source against the gateway configuration.
///
/// Returns a disabled policy (gating off, allow everything) when:
/// - `gateway_config` is None or null
/// - `source` is None
/// - the platform has no platform configuration
/// - the platform configuration has no admin list set for the resolved scope
pub fn policy_for_source(
    gateway_config: Option<&Value>,
    source: Option<&SessionSource>,
) -> SlashAccessPolicy {
    let (Some(config), Some(source)) = (gateway_config, source) else {
        return SlashAccessPolicy::disabled();
    };

    let platform_key = source.platform.as_deref().unwrap_or("");
    if platform_key.is_empty() {
        return SlashAccessPolicy::disabled();
    }

    let platform_config = config.as_object().and_then(|root| {
        if let Some(platforms) = root.get("platforms").and_then(|p| p.as_object()) {
            platforms
                .get(platform_key)
                .or_else(|| platforms.get(&platform_key.to_ascii_lowercase()))
        } else {
            root.get(platform_key)
                .or_else(|| root.get(&platform_key.to_ascii_lowercase()))
        }
    });

    let extra = platform_extra(platform_config);
    let scope = Scope::from_chat_type(source.chat_type.as_deref());
    policy_from_extra(&extra, scope)
}

/// Resolve the access policy when session source is provided as a raw [`Value`].
pub fn policy_for_source_value(
    gateway_config: Option<&Value>,
    source: Option<&Value>,
) -> SlashAccessPolicy {
    let (Some(config), Some(source)) = (gateway_config, source) else {
        return SlashAccessPolicy::disabled();
    };

    let source_obj = source.as_object();
    let platform = source_obj.and_then(|s| s.get("platform")).and_then(|p| match p {
        Value::String(s) => Some(s.clone()),
        _ => None,
    });
    let chat_type = source_obj.and_then(|s| s.get("chat_type")).and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        _ => None,
    });
    let user_id = source_obj.and_then(|s| s.get("user_id")).and_then(|u| match u {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });

    let src = SessionSource {
        platform,
        chat_id: None,
        chat_name: None,
        chat_type,
        user_id,
        user_name: None,
    };
    policy_for_source(Some(config), Some(&src))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_extra_is_disabled() {
        let p = policy_from_extra(&json!({}), Scope::Dm);
        assert!(!p.enabled);
        assert!(p.admin_user_ids.is_empty());
        assert!(p.user_allowed_commands.is_empty());
    }

    #[test]
    fn disabled_policy_treats_anyone_as_admin() {
        let p = policy_from_extra(&json!({}), Scope::Dm);
        assert!(p.is_admin(Some("anyone")));
        assert!(p.is_admin(None));
        assert!(p.can_run(Some("anyone"), "stop"));
        assert!(p.can_run(None, "stop"));
    }

    #[test]
    fn id_coercion_ints_and_strings() {
        let p = policy_from_extra(&json!({"allow_admin_from": [12345, 67890]}), Scope::Dm);
        assert_eq!(
            p.admin_user_ids,
            HashSet::from(["12345".to_string(), "67890".to_string()])
        );
        assert!(p.is_admin(Some("12345")));
        assert!(p.is_admin(Some("67890")));
        assert!(!p.is_admin(Some("99999")));

        // Comma-separated string format
        let p_csv = policy_from_extra(&json!({"allow_admin_from": "111, 222 , 333"}), Scope::Dm);
        assert!(p_csv.is_admin(Some("111")));
        assert!(p_csv.is_admin(Some("222")));
        assert!(p_csv.is_admin(Some("333")));
        assert!(!p_csv.is_admin(Some("444")));

        // Single scalar number
        let p_single = policy_from_extra(&json!({"allow_admin_from": 555}), Scope::Dm);
        assert!(p_single.is_admin(Some("555")));
    }

    #[test]
    fn command_coercion_strips_leading_slash_and_lowercases() {
        let p = policy_from_extra(
            &json!({
                "allow_admin_from": ["111"],
                "user_allowed_commands": ["/Status", "MODEL", "/help", "///restart"]
            }),
            Scope::Dm,
        );
        assert_eq!(
            p.user_allowed_commands,
            HashSet::from([
                "status".to_string(),
                "model".to_string(),
                "help".to_string(),
                "restart".to_string(),
            ])
        );
    }

    #[test]
    fn scope_resolution_for_chat_types() {
        assert_eq!(Scope::from_chat_type(Some("dm")), Scope::Dm);
        assert_eq!(Scope::from_chat_type(Some("DM")), Scope::Dm);
        assert_eq!(Scope::from_chat_type(Some("direct")), Scope::Dm);
        assert_eq!(Scope::from_chat_type(Some("private")), Scope::Dm);

        assert_eq!(Scope::from_chat_type(Some("group")), Scope::Group);
        assert_eq!(Scope::from_chat_type(Some("channel")), Scope::Group);
        assert_eq!(Scope::from_chat_type(Some("thread")), Scope::Group);
        assert_eq!(Scope::from_chat_type(Some("")), Scope::Group);
        assert_eq!(Scope::from_chat_type(Some("   ")), Scope::Group);
        assert_eq!(Scope::from_chat_type(None), Scope::Group);
    }

    #[test]
    fn dm_admin_does_not_imply_group_admin() {
        let extra = json!({"allow_admin_from": ["111"]});
        let dm = policy_from_extra(&extra, Scope::Dm);
        let gp = policy_from_extra(&extra, Scope::Group);

        assert!(dm.is_admin(Some("111")));
        // Group has no admin list set -> gating disabled -> unrestricted fallback
        assert!(!gp.enabled);
        assert!(gp.is_admin(Some("111")));

        // When group has its own distinct admin list
        let extra2 = json!({
            "allow_admin_from": ["111"],
            "group_allow_admin_from": ["222"]
        });
        let gp2 = policy_from_extra(&extra2, Scope::Group);
        assert!(gp2.enabled);
        assert!(!gp2.is_admin(Some("111")));
        assert!(gp2.is_admin(Some("222")));
    }

    #[test]
    fn dm_falls_back_to_group_user_allowed_commands() {
        let extra = json!({
            "allow_admin_from": ["111"],
            "group_user_allowed_commands": ["status", "model"]
        });
        let dm = policy_from_extra(&extra, Scope::Dm);
        assert!(dm.enabled);
        assert!(dm.user_allowed_commands.contains("status"));
        assert!(dm.user_allowed_commands.contains("model"));

        // Explicit DM commands take precedence and do not merge with group commands
        let extra_explicit = json!({
            "allow_admin_from": ["111"],
            "user_allowed_commands": ["custom"],
            "group_user_allowed_commands": ["status", "model"]
        });
        let dm_explicit = policy_from_extra(&extra_explicit, Scope::Dm);
        assert!(dm_explicit.user_allowed_commands.contains("custom"));
        assert!(!dm_explicit.user_allowed_commands.contains("status"));
    }

    #[test]
    fn always_allowed_floor_accessible_to_non_admins() {
        let p = policy_from_extra(
            &json!({
                "allow_admin_from": ["111"],
                "user_allowed_commands": []
            }),
            Scope::Dm,
        );
        assert!(p.enabled);
        assert!(!p.is_admin(Some("999")));

        // Floor commands (canonical form) are always permitted.
        assert!(p.can_run(Some("999"), "help"));
        assert!(p.can_run(Some("999"), "whoami"));
        // can_run does not normalize: a non-canonical "/help" is NOT the floor
        // command "help" (Python compares the caller's canonical string as-is).
        assert!(!p.can_run(Some("999"), "/help"));

        // Other commands denied
        assert!(!p.can_run(Some("999"), "stop"));
        assert!(!p.can_run(Some("999"), "status"));

        // Admin can run anything
        assert!(p.can_run(Some("111"), "stop"));
    }

    #[test]
    fn can_run_empty_command_is_denied() {
        let p = policy_from_extra(&json!({"allow_admin_from": ["111"]}), Scope::Dm);
        assert!(!p.can_run(Some("999"), ""));
        assert!(!p.can_run(Some("999"), "   "));
        assert!(!p.can_run(Some("999"), "/"));
    }

    #[test]
    fn is_admin_handles_empty_and_none() {
        let p = policy_from_extra(&json!({"allow_admin_from": ["111"]}), Scope::Dm);
        assert!(!p.is_admin(None));
        assert!(!p.is_admin(Some("")));
        assert!(!p.is_admin(Some("   ")));
    }

    #[test]
    fn policy_for_source_dm_resolves_correctly() {
        let cfg = json!({
            "platforms": {
                "discord": {
                    "enabled": true,
                    "extra": {
                        "allow_admin_from": ["111"],
                        "user_allowed_commands": ["status"],
                        "group_allow_admin_from": ["222"],
                        "group_user_allowed_commands": ["help"]
                    }
                }
            }
        });

        let dm_src = SessionSource::new("discord", "A", "dm", Some("111"));
        let p = policy_for_source(Some(&cfg), Some(&dm_src));

        assert!(p.enabled);
        assert!(p.is_admin(Some("111")));
        assert!(p.can_run(Some("999"), "status"));
        assert!(p.can_run(Some("999"), "help")); // always-allowed floor
        assert!(!p.can_run(Some("999"), "kanban"));
    }

    #[test]
    fn policy_for_source_ungated_scope_is_unrestricted() {
        let cfg = json!({
            "platforms": {
                "discord": {
                    "enabled": true,
                    "extra": {
                        "group_allow_admin_from": ["222"]
                    }
                }
            }
        });

        let dm_src = SessionSource::new("discord", "A", "dm", Some("999"));
        let grp_src = SessionSource::new("discord", "G", "group", Some("999"));

        let dm_p = policy_for_source(Some(&cfg), Some(&dm_src));
        let grp_p = policy_for_source(Some(&cfg), Some(&grp_src));

        assert!(!dm_p.enabled);
        assert!(dm_p.can_run(Some("999"), "stop")); // backward compat

        assert!(grp_p.enabled);
        assert!(!grp_p.can_run(Some("999"), "stop")); // gated
    }

    #[test]
    fn policy_for_source_missing_config_returns_disabled() {
        let src = SessionSource::new("discord", "A", "dm", Some("111"));
        let p1 = policy_for_source(None, Some(&src));
        assert!(!p1.enabled);

        let cfg = json!({"platforms": {}});
        let p2 = policy_for_source(Some(&cfg), None);
        assert!(!p2.enabled);
    }

    #[test]
    fn policy_for_source_value_helper() {
        let cfg = json!({
            "platforms": {
                "telegram": {
                    "extra": {
                        "allow_admin_from": ["555"],
                        "user_allowed_commands": ["model"]
                    }
                }
            }
        });
        let src_val = json!({
            "platform": "telegram",
            "chat_type": "dm",
            "user_id": "555"
        });

        let p = policy_for_source_value(Some(&cfg), Some(&src_val));
        assert!(p.enabled);
        assert!(p.is_admin(Some("555")));
        assert!(p.can_run(Some("999"), "model"));
        assert!(!p.can_run(Some("999"), "stop"));
    }
}
