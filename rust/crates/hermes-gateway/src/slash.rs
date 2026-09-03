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

use crate::slash_access::{policy_for_source, SessionSource};

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

/// Gate an inbound message against the slash-access policy in `user_config`.
pub fn evaluate(user_config: &Value, msg: &Message) -> SlashDecision {
    let Some(command) = command_name(&msg.text) else {
        return SlashDecision::NotSlash;
    };

    let source = SessionSource::from_platform(
        msg.platform,
        msg.channel_id.clone(),
        msg.chat_type.clone().unwrap_or_default(),
        Some(msg.sender_id.clone()),
    );
    let policy = policy_for_source(Some(user_config), Some(&source));

    if policy.can_run(Some(&msg.sender_id), &command) {
        SlashDecision::Allowed { command }
    } else {
        SlashDecision::Denied { command }
    }
}

/// The user-facing refusal text for a denied command.
pub fn denial_text(command: &str) -> String {
    format!("⛔ You are not allowed to run /{command} here.")
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
        assert_eq!(evaluate(&cfg, &msg("just chatting", "u", "private")), SlashDecision::NotSlash);
    }

    #[test]
    fn ungated_platform_allows_everything() {
        // No admin list configured for telegram -> gating disabled -> allow.
        let cfg = json!({"platforms": {"telegram": {"extra": {}}}});
        assert_eq!(
            evaluate(&cfg, &msg("/deploy", "u", "private")),
            SlashDecision::Allowed { command: "deploy".into() }
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
            SlashDecision::Denied { command: "deploy".into() }
        );
        // Allowlisted command is permitted.
        assert_eq!(
            evaluate(&cfg, &msg("/status", "111", "private")),
            SlashDecision::Allowed { command: "status".into() }
        );
        // The always-allowed floor (help/whoami) is permitted for non-admins.
        assert_eq!(
            evaluate(&cfg, &msg("/help", "111", "private")),
            SlashDecision::Allowed { command: "help".into() }
        );
        // Admin runs anything.
        assert_eq!(
            evaluate(&cfg, &msg("/deploy", "999", "private")),
            SlashDecision::Allowed { command: "deploy".into() }
        );
    }
}
