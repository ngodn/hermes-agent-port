//! Port of gateway/relay/command_manifest.py.
//!
// Public API is ahead of its callers (the hello frame wires it in a later slice).
#![allow(dead_code)]
//!
//! Gateway-declared slash-command manifest for the relay lane.
//!
//! The native Discord adapter registers its slash commands directly on the
//! Discord command tree because it holds the bot token. Over the relay the
//! CONNECTOR holds the token, so the gateway DECLARES the same command set on
//! its `hello` frame (`command_manifest`) and the connector reconciles
//! Discord's global application-command registration against it (GET, diff,
//! bulk PUT, idempotent, best-effort).
//!
//! This module is that declaration: the single source of truth for what the
//! relay lane advertises. It mirrors the native tree (same names, same
//! descriptions) so a user moving between a native-Discord deployment and a
//! hosted/relay one sees the same command palette. Interactions come back over
//! the passthrough plane and are normalized into the same "/name args" COMMAND
//! events the dispatcher already routes, so declaring a command here requires
//! no new handler.
//!
//! Wire shape (per entry): {name, description, options?} where options rows are
//! Discord option objects passed through verbatim. Names must satisfy Discord's
//! CHAT_INPUT rules ([a-z0-9_-]{1,32}); the connector drops invalid entries
//! (fail-open per entry, never the whole manifest).
//!
//! Field order matters for a byte-exact match with the Python `json.dumps`
//! output, so the wire types below are serde structs (fields serialize in
//! declaration order), not `serde_json::Value` maps (which sort keys because
//! this workspace has no `preserve_order` feature).

use serde::Serialize;

// Discord option type 3 = STRING.
const STR: i64 = 3;

/// One `{name, value}` choice row on a string option.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Choice {
    pub name: String,
    pub value: String,
}

/// A Discord slash-command option object (string options only, as the native
/// tree declares them here).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandOption {
    #[serde(rename = "type")]
    pub type_: i64,
    pub name: String,
    pub description: String,
    pub required: bool,
    // Omitted entirely when empty, matching Python's `if choices:` guard (an
    // empty list is falsy, so no "choices" key is written).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
}

/// One command entry in the manifest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandEntry {
    pub name: String,
    pub description: String,
    // Omitted when the command takes no options, matching the Python entries
    // that simply don't carry an "options" key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<CommandOption>,
}

impl CommandEntry {
    /// A bare command with no options.
    fn bare(name: &str, description: &str) -> Self {
        CommandEntry {
            name: name.to_string(),
            description: description.to_string(),
            options: Vec::new(),
        }
    }

    /// A command with a single option.
    fn with_option(name: &str, description: &str, option: CommandOption) -> Self {
        CommandEntry {
            name: name.to_string(),
            description: description.to_string(),
            options: vec![option],
        }
    }
}

/// Port of `_opt`: a STRING option, not required, with optional choices.
///
/// Pass an empty slice for `choices` when the option has none; that mirrors the
/// Python default of `None`/empty and results in no "choices" key on the wire.
fn opt(name: &str, description: &str, choices: &[&str]) -> CommandOption {
    CommandOption {
        type_: STR,
        name: name.to_string(),
        description: description.to_string(),
        required: false,
        choices: choices
            .iter()
            .map(|c| Choice {
                name: (*c).to_string(),
                value: (*c).to_string(),
            })
            .collect(),
    }
}

/// The relay lane's Discord slash-command manifest (native-tree mirror).
pub fn build_relay_command_manifest() -> Vec<CommandEntry> {
    vec![
        CommandEntry::bare("new", "Start a new conversation"),
        CommandEntry::bare("reset", "Reset your Hermes session"),
        CommandEntry::with_option(
            "model",
            "Show or change the model",
            opt("name", "Model name. Leave empty to see current.", &[]),
        ),
        CommandEntry::with_option(
            "reasoning",
            "Show/change reasoning effort, or toggle showing it",
            opt(
                "effort",
                "Level, reset, or show/hide. Leave empty to see current.",
                &[
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "reset",
                    "show", "hide",
                ],
            ),
        ),
        CommandEntry::with_option(
            "personality",
            "Set a personality",
            opt("name", "Personality name. Leave empty to list.", &[]),
        ),
        CommandEntry::bare("retry", "Retry your last message"),
        CommandEntry::bare("undo", "Remove the last exchange"),
        CommandEntry::bare("status", "Show Hermes session status"),
        CommandEntry::bare("sethome", "Set this chat as the home channel"),
        CommandEntry::bare("stop", "Stop the running Hermes agent"),
        CommandEntry::with_option(
            "steer",
            "Inject a message after the next tool call (no interrupt)",
            opt("text", "What to tell the agent", &[]),
        ),
        CommandEntry::bare("compress", "Compress conversation context"),
        CommandEntry::with_option(
            "title",
            "Set or show the session title",
            opt("text", "New title. Leave empty to show.", &[]),
        ),
        CommandEntry::with_option(
            "resume",
            "Resume a previously-named session",
            opt("name", "Session title or id", &[]),
        ),
        CommandEntry::bare("usage", "Show token usage for this session"),
        CommandEntry::bare("help", "Show available commands"),
        CommandEntry::bare("insights", "Show usage insights and analytics"),
        CommandEntry::bare("reload-mcp", "Reload MCP servers from config"),
        CommandEntry::bare("reload-skills", "Re-scan skills for new or removed entries"),
        CommandEntry::bare("voice", "Toggle voice reply mode"),
        CommandEntry::bare("update", "Update Hermes Agent to the latest version"),
        CommandEntry::bare("restart", "Gracefully restart the Hermes gateway"),
        CommandEntry::with_option(
            "approve",
            "Approve a pending dangerous command",
            opt(
                "scope",
                "Approval scope",
                &["once", "session", "always", "all"],
            ),
        ),
        CommandEntry::with_option(
            "deny",
            "Deny a pending dangerous command",
            opt("reason", "Why (relayed to the agent)", &[]),
        ),
        CommandEntry::with_option(
            "thread",
            "Create a new thread and start a Hermes session in it",
            opt("name", "Thread name", &[]),
        ),
        CommandEntry::with_option(
            "queue",
            "Queue a prompt for the next turn (doesn't interrupt)",
            opt("text", "The prompt to queue", &[]),
        ),
        CommandEntry::with_option(
            "bg",
            "Run a prompt in a separate background session",
            opt("text", "The prompt to run", &[]),
        ),
        CommandEntry::with_option(
            "btw",
            "Ask a side question about the current conversation",
            opt("text", "The question to answer", &[]),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Locked byte-for-byte against the reference Python, compact separators so
    // it matches serde_json::to_string exactly (no spaces after ':' or ','):
    //   python3 -c "import json; from gateway.relay.command_manifest import \
    //     build_relay_command_manifest as b; \
    //     print(json.dumps(b(), separators=(',', ':')))"
    const EXPECTED_JSON: &str = r#"[{"name":"new","description":"Start a new conversation"},{"name":"reset","description":"Reset your Hermes session"},{"name":"model","description":"Show or change the model","options":[{"type":3,"name":"name","description":"Model name. Leave empty to see current.","required":false}]},{"name":"reasoning","description":"Show/change reasoning effort, or toggle showing it","options":[{"type":3,"name":"effort","description":"Level, reset, or show/hide. Leave empty to see current.","required":false,"choices":[{"name":"none","value":"none"},{"name":"minimal","value":"minimal"},{"name":"low","value":"low"},{"name":"medium","value":"medium"},{"name":"high","value":"high"},{"name":"xhigh","value":"xhigh"},{"name":"max","value":"max"},{"name":"ultra","value":"ultra"},{"name":"reset","value":"reset"},{"name":"show","value":"show"},{"name":"hide","value":"hide"}]}]},{"name":"personality","description":"Set a personality","options":[{"type":3,"name":"name","description":"Personality name. Leave empty to list.","required":false}]},{"name":"retry","description":"Retry your last message"},{"name":"undo","description":"Remove the last exchange"},{"name":"status","description":"Show Hermes session status"},{"name":"sethome","description":"Set this chat as the home channel"},{"name":"stop","description":"Stop the running Hermes agent"},{"name":"steer","description":"Inject a message after the next tool call (no interrupt)","options":[{"type":3,"name":"text","description":"What to tell the agent","required":false}]},{"name":"compress","description":"Compress conversation context"},{"name":"title","description":"Set or show the session title","options":[{"type":3,"name":"text","description":"New title. Leave empty to show.","required":false}]},{"name":"resume","description":"Resume a previously-named session","options":[{"type":3,"name":"name","description":"Session title or id","required":false}]},{"name":"usage","description":"Show token usage for this session"},{"name":"help","description":"Show available commands"},{"name":"insights","description":"Show usage insights and analytics"},{"name":"reload-mcp","description":"Reload MCP servers from config"},{"name":"reload-skills","description":"Re-scan skills for new or removed entries"},{"name":"voice","description":"Toggle voice reply mode"},{"name":"update","description":"Update Hermes Agent to the latest version"},{"name":"restart","description":"Gracefully restart the Hermes gateway"},{"name":"approve","description":"Approve a pending dangerous command","options":[{"type":3,"name":"scope","description":"Approval scope","required":false,"choices":[{"name":"once","value":"once"},{"name":"session","value":"session"},{"name":"always","value":"always"},{"name":"all","value":"all"}]}]},{"name":"deny","description":"Deny a pending dangerous command","options":[{"type":3,"name":"reason","description":"Why (relayed to the agent)","required":false}]},{"name":"thread","description":"Create a new thread and start a Hermes session in it","options":[{"type":3,"name":"name","description":"Thread name","required":false}]},{"name":"queue","description":"Queue a prompt for the next turn (doesn't interrupt)","options":[{"type":3,"name":"text","description":"The prompt to queue","required":false}]},{"name":"bg","description":"Run a prompt in a separate background session","options":[{"type":3,"name":"text","description":"The prompt to run","required":false}]},{"name":"btw","description":"Ask a side question about the current conversation","options":[{"type":3,"name":"text","description":"The question to answer","required":false}]}]"#;

    #[test]
    fn manifest_serializes_byte_exact_to_python() {
        let manifest = build_relay_command_manifest();
        let got = serde_json::to_string(&manifest).unwrap();
        assert_eq!(got, EXPECTED_JSON);
    }

    #[test]
    fn entry_count_and_first_last() {
        let m = build_relay_command_manifest();
        assert_eq!(m.len(), 28);
        assert_eq!(m[0].name, "new");
        assert_eq!(m[0].description, "Start a new conversation");
        assert!(m[0].options.is_empty());
        assert_eq!(m.last().unwrap().name, "btw");
    }

    #[test]
    fn bare_entries_omit_options_key() {
        // A bare command must not serialize an "options" field at all.
        let entry = CommandEntry::bare("new", "Start a new conversation");
        let s = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            s,
            r#"{"name":"new","description":"Start a new conversation"}"#
        );
        assert!(!s.contains("options"));
    }

    #[test]
    fn option_without_choices_omits_choices_key() {
        let o = opt("name", "Model name. Leave empty to see current.", &[]);
        let s = serde_json::to_string(&o).unwrap();
        assert!(!s.contains("choices"));
        assert_eq!(o.type_, 3);
        assert!(!o.required);
    }

    #[test]
    fn option_with_choices_mirrors_name_into_value() {
        let o = opt(
            "scope",
            "Approval scope",
            &["once", "session", "always", "all"],
        );
        assert_eq!(o.choices.len(), 4);
        assert_eq!(o.choices[0].name, "once");
        assert_eq!(o.choices[0].value, "once");
        assert_eq!(o.choices[3].name, "all");
        assert_eq!(o.choices[3].value, "all");
    }

    #[test]
    fn reasoning_choices_order_preserved() {
        let m = build_relay_command_manifest();
        let reasoning = m.iter().find(|e| e.name == "reasoning").unwrap();
        let choices = &reasoning.options[0].choices;
        let names: Vec<&str> = choices.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra", "reset",
                "show", "hide"
            ]
        );
    }
}
