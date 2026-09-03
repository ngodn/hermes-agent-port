//! Slash-command gating at the delivery boundary.
//!
//! Bridges an inbound [`Message`] to the ported [`slash_access`] policy: it
//! detects a leading slash command, resolves the per-platform/per-scope policy
//! from the user config, and decides whether this sender may run it. This is
//! the gate the Python applies at the slash dispatch site
//! (`gateway/run.py`), minus (for now) the actual command handlers: an allowed
//! command still flows to the agent as before, a denied one is refused.

use hermes_core::Message;
use serde_json::Value;

use crate::slash_access::{policy_for_source, Scope, SessionSource, SlashAccessPolicy};

/// Built-in commands the gateway answers itself, without spending an agent turn.
pub const BUILTIN_COMMANDS: &[&str] = &["help", "whoami", "status"];

/// Outcome of gating an inbound message.
#[derive(Debug, PartialEq, Eq)]
pub enum SlashDecision {
    /// Not a slash command; deliver to the agent normally.
    NotSlash,
    /// A slash command this sender is allowed to run.
    Allowed { command: String },
    /// A slash command this sender may not run.
    Denied { command: String },
}

/// Extract the canonical command name from message text.
///
/// Returns `None` when the text is not a slash command. Handles the Telegram
/// `/cmd@botname args` shape: the leading `/` is required, the name runs to the
/// first whitespace, an `@mention` suffix is dropped, and the result is
/// lowercased to match how the allowlist is canonicalized.
pub fn command_name(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let rest = trimmed.strip_prefix('/')?;
    // Command token ends at the first whitespace.
    let token = rest.split_whitespace().next().unwrap_or("");
    // Drop a Telegram @botname suffix.
    let name = token.split('@').next().unwrap_or("");
    if name.is_empty() {
        return None;
    }
    Some(name.to_ascii_lowercase())
}

/// Build the [`SessionSource`] for an inbound message.
fn source_for(msg: &Message) -> SessionSource {
    SessionSource::from_platform(
        msg.platform,
        msg.channel_id.clone(),
        msg.chat_type.clone().unwrap_or_default(),
        Some(msg.sender_id.clone()),
    )
}

/// Resolve the slash-access policy for an inbound message.
fn policy_for(user_config: &Value, msg: &Message) -> SlashAccessPolicy {
    policy_for_source(Some(user_config), Some(&source_for(msg)))
}

/// Gate an inbound message against the slash-access policy in `user_config`.
pub fn evaluate(user_config: &Value, msg: &Message) -> SlashDecision {
    let Some(command) = command_name(&msg.text) else {
        return SlashDecision::NotSlash;
    };

    if policy_for(user_config, msg).can_run(Some(&msg.sender_id), &command) {
        SlashDecision::Allowed { command }
    } else {
        SlashDecision::Denied { command }
    }
}

/// The user-facing refusal text for a denied command.
pub fn denial_text(command: &str) -> String {
    format!("⛔ You are not allowed to run /{command} here.")
}

/// Answer a built-in command directly, without spending an agent turn. Returns
/// `None` for any command that is not a gateway built-in (it then flows to the
/// agent). Callers invoke this only after the command is gate-allowed.
pub fn handle_builtin(command: &str, msg: &Message, user_config: &Value) -> Option<String> {
    match command {
        "help" => Some(help_text(msg, user_config)),
        "whoami" => Some(whoami_text(msg, user_config)),
        "status" => Some(status_text()),
        _ => None,
    }
}

fn help_text(msg: &Message, user_config: &Value) -> String {
    let policy = policy_for(user_config, msg);
    let mut out = String::from("Available commands:\n");
    out.push_str("  /help    show this message\n");
    out.push_str("  /whoami  show your identity and access\n");
    out.push_str("  /status  show gateway status\n");
    out.push_str("Any other message is handled by the agent.");
    if policy.enabled && !policy.is_admin(Some(&msg.sender_id)) {
        // Non-admins on a gated platform: show the extra commands they may run.
        let mut extra: Vec<&str> = policy
            .user_allowed_commands
            .iter()
            .map(String::as_str)
            .filter(|c| !BUILTIN_COMMANDS.contains(c))
            .collect();
        extra.sort_unstable();
        if !extra.is_empty() {
            out.push_str("\nYou may also run: ");
            out.push_str(
                &extra
                    .iter()
                    .map(|c| format!("/{c}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
    out
}

fn whoami_text(msg: &Message, user_config: &Value) -> String {
    let policy = policy_for(user_config, msg);
    let scope = Scope::from_chat_type(msg.chat_type.as_deref());
    let access = if !policy.enabled {
        "unrestricted (gating off)"
    } else if policy.is_admin(Some(&msg.sender_id)) {
        "admin"
    } else {
        "user"
    };
    format!(
        "platform: {:?}\nchat: {} ({})\nuser id: {}\naccess: {}",
        msg.platform, msg.channel_id, scope, msg.sender_id, access
    )
}

fn status_text() -> String {
    format!("Hermes gateway v{} online.", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::Platform;
    use serde_json::json;

    fn msg(text: &str, sender: &str, chat_type: &str) -> Message {
        Message {
            platform: Platform::Telegram,
            channel_id: "c".into(),
            sender_id: sender.into(),
            text: text.into(),
            chat_type: Some(chat_type.into()),
        }
    }

    #[test]
    fn parses_command_names() {
        assert_eq!(command_name("/help"), Some("help".into()));
        assert_eq!(command_name("  /Status now"), Some("status".into()));
        assert_eq!(command_name("/help@MyBot arg"), Some("help".into()));
        assert_eq!(command_name("hello"), None);
        assert_eq!(command_name("/"), None);
        assert_eq!(command_name(""), None);
    }

    #[test]
    fn non_slash_passes_through() {
        let cfg = json!({});
        assert_eq!(
            evaluate(&cfg, &msg("just chatting", "u", "private")),
            SlashDecision::NotSlash
        );
    }

    #[test]
    fn ungated_platform_allows_everything() {
        // No admin list configured for telegram -> gating disabled -> allow.
        let cfg = json!({"platforms": {"telegram": {"extra": {}}}});
        assert_eq!(
            evaluate(&cfg, &msg("/deploy", "u", "private")),
            SlashDecision::Allowed {
                command: "deploy".into()
            }
        );
    }

    #[test]
    fn gated_platform_denies_non_admin_non_allowlisted() {
        let cfg = json!({
            "platforms": {"telegram": {"extra": {
                "allow_admin_from": ["999"],
                "user_allowed_commands": ["status"]
            }}}
        });
        // Non-admin running an unlisted command is denied.
        assert_eq!(
            evaluate(&cfg, &msg("/deploy", "111", "private")),
            SlashDecision::Denied {
                command: "deploy".into()
            }
        );
        // Allowlisted command is permitted.
        assert_eq!(
            evaluate(&cfg, &msg("/status", "111", "private")),
            SlashDecision::Allowed {
                command: "status".into()
            }
        );
        // The always-allowed floor (help/whoami) is permitted for non-admins.
        assert_eq!(
            evaluate(&cfg, &msg("/help", "111", "private")),
            SlashDecision::Allowed {
                command: "help".into()
            }
        );
        // Admin runs anything.
        assert_eq!(
            evaluate(&cfg, &msg("/deploy", "999", "private")),
            SlashDecision::Allowed {
                command: "deploy".into()
            }
        );
    }

    #[test]
    fn builtins_answered_directly() {
        let cfg = json!({});
        let m = msg("/status", "u", "private");
        let s = handle_builtin("status", &m, &cfg).unwrap();
        assert!(s.contains("online"));

        let w = handle_builtin("whoami", &m, &cfg).unwrap();
        assert!(w.contains("unrestricted")); // no gating configured
        assert!(w.contains("u"));

        let h = handle_builtin("help", &m, &cfg).unwrap();
        assert!(h.contains("/help") && h.contains("/whoami") && h.contains("/status"));

        // Non-built-ins fall through to the agent.
        assert_eq!(handle_builtin("deploy", &m, &cfg), None);
    }

    #[test]
    fn whoami_reports_admin_and_help_lists_allowed() {
        let cfg = json!({
            "platforms": {"telegram": {"extra": {
                "allow_admin_from": ["999"],
                "user_allowed_commands": ["deploy", "status"]
            }}}
        });
        // DM scope: the config gates the dm-scope keys (allow_admin_from).
        let admin = handle_builtin("whoami", &msg("/whoami", "999", "private"), &cfg).unwrap();
        assert!(admin.contains("admin"));

        let user_help = handle_builtin("help", &msg("/help", "111", "private"), &cfg).unwrap();
        // The extra allowlisted non-builtin command is surfaced; status (a
        // built-in) is not duplicated in the "also run" line.
        assert!(user_help.contains("/deploy"));
    }
}
