//! Slack platform adapter (Socket Mode).
//!
//! The third real [`PlatformAdapter`]. Slack Socket Mode delivers events over a
//! WebSocket whose URL is minted at runtime: POST `apps.connections.open` with
//! the app-level token to get a `wss://` URL, connect, receive `hello`, then
//! handle `events_api` envelopes, acking each by `envelope_id`. Outbound uses
//! `chat.postMessage` with the bot token. Built from the documented contract
//! (https://docs.slack.dev/apis/events-api/using-socket-mode), not a port of
//! the Python adapter.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hermes_core::{Error, Message, Platform, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::platform::PlatformAdapter;

const API_BASE: &str = "https://slack.com/api";

/// A decoded Socket Mode envelope.
#[derive(Debug, PartialEq)]
pub struct Envelope {
    /// Present on messages that must be acknowledged.
    pub envelope_id: Option<String>,
    /// "hello", "events_api", "disconnect", ...
    pub kind: String,
    pub payload: Value,
}

/// Decode a Socket Mode frame into an [`Envelope`].
pub fn parse_envelope(v: &Value) -> Option<Envelope> {
    let kind = v.get("type").and_then(Value::as_str)?.to_string();
    Some(Envelope {
        envelope_id: v
            .get("envelope_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        kind,
        payload: v.get("payload").cloned().unwrap_or(Value::Null),
    })
}

/// The ack frame for an envelope id.
pub fn ack_payload(envelope_id: &str) -> Value {
    json!({ "envelope_id": envelope_id })
}

/// Map an `events_api` payload to a [`Message`].
///
/// Handles plain and file-share user messages. Bot events and other subtypes
/// (edits, joins, bot posts) are skipped to avoid loops and noise. `channel_type`
/// "im" is a DM; anything else is a group/channel, for access scope.
#[cfg(test)]
fn parse_message_event(payload: &Value) -> Option<Message> {
    if declares_bot(&payload["event"], &Default::default()) {
        return None;
    }
    parse_permitted_message(payload, "", false)
}

/// Slack does not support native slash commands in threads. Rewrite only
/// registry-recognized bang commands; preserve the original token and arguments.
fn rewrite_bang(text: &str) -> String {
    let Some(rest) = text.strip_prefix('!') else {
        return text.into();
    };
    let name = rest
        .split(crate::python_value::python_whitespace)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("")
        .to_lowercase();
    if !name.is_empty() && !name.contains('/') && crate::command_catalog::gateway_knows(&name) {
        format!("/{rest}")
    } else {
        text.into()
    }
}

fn parse_permitted_message(payload: &Value, bot: &str, pattern_mentioned: bool) -> Option<Message> {
    let event = payload.get("event")?;
    if !eligible_message(event) {
        return None;
    }
    let mut text = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let probe = text.trim_start_matches(crate::python_value::python_whitespace);
    let rewritten = rewrite_bang(probe);
    if rewritten != probe {
        text = rewritten;
    }
    let canonical_text = text.clone();
    // Commands must retain their original arguments, including after a bang
    // rewrite. Merge authored rich text and UI context only for ordinary turns.
    if !text
        .trim_start_matches(crate::python_value::python_whitespace)
        .starts_with('/')
    {
        let additional = crate::slack_blocks::additional_text(&event["blocks"], &text, bot);
        if !additional.is_empty() {
            text = format!(
                "{}\n{additional}",
                text.trim_matches(crate::python_value::python_whitespace)
            )
            .trim_matches(crate::python_value::python_whitespace)
            .to_owned();
        }
        let blocks = crate::slack_blocks::serialize_for_agent(&event["blocks"], 6000);
        if !blocks.is_empty() {
            text = format!(
                "{}\n\n{blocks}",
                text.trim_matches(crate::python_value::python_whitespace)
            )
            .trim_matches(crate::python_value::python_whitespace)
            .to_owned();
        }
    }
    if !text
        .trim_start_matches(crate::python_value::python_whitespace)
        .starts_with('/')
    {
        text = crate::slack_blocks::append_attachments(&text, &event["attachments"]);
    }
    if pattern_mentioned || (!bot.is_empty() && mentions_user(event, bot)) {
        let mention = format!("<@{bot}>");
        text = text
            .replace(&mention, "")
            .trim_matches(crate::python_value::python_whitespace)
            .to_owned();
        // A leading mention can hide a command from the first probe. Recover
        // arguments from the canonical event text, never from enriched blocks
        // or attachment previews, which may contain quoted commands.
        let stripped = canonical_text.replace(&mention, "");
        let command = rewrite_bang(stripped.trim_matches(crate::python_value::python_whitespace));
        if command.starts_with('/') {
            text = command;
        }
    }
    if text.is_empty()
        && !event["files"].as_array().is_some_and(|files| {
            files
                .iter()
                .any(|file| audio_extension(file).is_some() || video_extension(file).is_some())
        })
    {
        return None;
    }
    let channel = event.get("channel").and_then(Value::as_str)?.to_string();
    let user = event.get("user").and_then(Value::as_str)?.to_string();
    let chat_type = if matches!(channel_kind(event), "im" | "mpim") {
        "dm"
    } else {
        "group"
    };
    Some(Message {
        resolved_session_id: None,
        platform: Platform::Slack,
        channel_id: channel,
        sender_id: user,
        text,
        content_parts: None,
        chat_type: Some(chat_type.to_string()),
        audio_paths: Vec::new(),
        video_paths: Vec::new(),
        workspace_id: event_team(payload),
        message_id: None,
        thread_id: None,
    })
}

/// Check identity and event kind before any authenticated metadata lookup.
fn eligible_message(event: &Value) -> bool {
    event["type"] == "message"
        && !event
            .get("subtype")
            .is_some_and(|s| !s.is_null() && s != "file_share" && s != "bot_message")
        && event["channel"].is_string()
        && event["user"].is_string()
}

fn channel_kind(event: &Value) -> &str {
    if !crate::python_value::truthy(&event["channel_type"])
        && event["channel"].as_str().unwrap_or("").starts_with('D')
    {
        "im"
    } else {
        event["channel_type"].as_str().unwrap_or("")
    }
}

/// Build the same source passed to Python's SessionStore for thread lookup.
/// MPIM remains a DM source even though its channel ID normally starts with G.
#[allow(dead_code)]
fn thread_session_key(
    event: &Value,
    team: &str,
    config: Option<&crate::config_gateway::GatewayConfig>,
    profile: Option<&str>,
) -> Option<String> {
    let config = config?;
    let mut source =
        crate::session::SessionSource::new("slack", event["channel"].as_str().unwrap_or(""));
    source.chat_type = if matches!(channel_kind(event), "im" | "mpim") {
        "dm"
    } else {
        "group"
    }
    .into();
    source.user_id = Some(event["user"].as_str().unwrap_or("").into());
    source.thread_id = Some(event["thread_ts"].as_str().unwrap_or("").into());
    source.scope_id = if team.is_empty() {
        None
    } else {
        Some(team.into())
    };
    Some(crate::session::build_session_key(
        &source,
        config.group_sessions_per_user,
        config.thread_sessions_per_user,
        profile,
    ))
}

fn disable_dms(config: &Value, legacy: Option<&str>) -> bool {
    let extra = &config["platforms"]["slack"]["extra"]["disable_dms"];
    let yaml = &config["slack"]["disable_dms"];
    let raw = if !extra.is_null() {
        extra.clone()
    } else if let Some(value) = legacy.filter(|s| !s.is_empty()) {
        json!(value)
    } else if !yaml.is_null() {
        json!(yaml
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(yaml)))
    } else {
        json!(legacy.unwrap_or("false"))
    };
    match raw.as_str() {
        Some(s) => matches!(
            s.trim_matches(crate::python_value::python_whitespace)
                .to_lowercase()
                .as_str(),
            "true" | "1" | "yes" | "on"
        ),
        None => crate::python_value::truthy(&raw),
    }
}

fn allowed_channels(config: &Value, legacy: Option<&str>) -> std::collections::BTreeSet<String> {
    let extra = &config["platforms"]["slack"]["extra"]["allowed_channels"];
    if !extra.is_null() && !extra.is_array() && !extra.is_string() {
        return Default::default();
    }
    // Reuse the source-compatible list/CSV parser and YAML environment bridge.
    let mapped = json!({"platforms":{"slack":{"extra":{"ignored_channels":extra}}},
        "slack":{"ignored_channels":config["slack"]["allowed_channels"]}});
    ignored_channels(&mapped, legacy)
}

fn bot_policy(config: &Value, legacy: Option<&str>) -> String {
    let extra = &config["platforms"]["slack"]["extra"]["allow_bots"];
    let yaml = &config["slack"]["allow_bots"];
    let raw = if crate::python_value::truthy(extra) {
        extra.clone()
    } else if let Some(value) = legacy.filter(|s| !s.is_empty()) {
        json!(value)
    } else if !yaml.is_null() {
        yaml.clone()
    } else {
        json!(legacy.unwrap_or("none"))
    };
    let value = raw
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::python_value::python_repr(&raw));
    match value
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase()
        .as_str()
    {
        "all" => "all",
        "mentions" => "mentions",
        _ => "none",
    }
    .into()
}

fn declares_bot(event: &Value, api_humans: &std::collections::BTreeSet<String>) -> bool {
    let truthy = crate::python_value::truthy;
    truthy(&event["bot_id"])
        || truthy(&event["bot_profile"])
        || event["subtype"] == "bot_message"
        || truthy(&event["user_profile"]["is_bot"])
        || (truthy(&event["app_id"])
            && !truthy(&event["client_msg_id"])
            && !event["user"]
                .as_str()
                .is_some_and(|u| api_humans.contains(u)))
}

/// Only authored user elements count. Quoted/forwarded Block Kit mentions do
/// not summon the bot, and unrelated fields are not recursively searched.
fn mention_detection_text(event: &Value) -> String {
    fn walk(node: &Value, mentions: &mut Vec<String>) {
        if let Some(items) = node.as_array() {
            for item in items {
                walk(item, mentions);
            }
            return;
        }
        if !node.is_object() || node["type"] == "rich_text_quote" {
            return;
        }
        if node["type"] == "user" && crate::python_value::truthy(&node["user_id"]) {
            let user = node["user_id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(&node["user_id"]));
            mentions.push(format!("<@{user}>"));
        }
        walk(&node["elements"], mentions);
        walk(&node["element"], mentions);
    }
    let flat = event["text"].as_str().unwrap_or("");
    let mut mentions = Vec::new();
    walk(&event["blocks"], &mut mentions);
    mentions.retain(|mention| !flat.contains(mention));
    if mentions.is_empty() {
        flat.to_owned()
    } else {
        format!(
            "{}\n{}",
            flat.trim_matches(crate::python_value::python_whitespace),
            mentions.join(" ")
        )
        .trim_matches(crate::python_value::python_whitespace)
        .to_owned()
    }
}

fn mentions_user(event: &Value, user: &str) -> bool {
    mention_detection_text(event).contains(&format!("<@{user}>"))
}

/// A leading user mention addresses that person. Pipe-form labels count here,
/// but room broadcasts and references in the middle of a sentence do not.
fn addressed_to_other(text: &str, self_users: &[&str]) -> bool {
    let text = text.trim_start_matches(crate::python_value::python_whitespace);
    let Some(rest) = text.strip_prefix("<@") else {
        return false;
    };
    let Some(end) = rest.find('>') else {
        return false;
    };
    let user = rest[..end].split('|').next().unwrap_or("");
    !user.is_empty()
        && !user.chars().any(crate::python_value::python_whitespace)
        && !self_users.contains(&user)
}

fn mentions_self(text: &str, self_users: &[&str]) -> bool {
    self_users
        .iter()
        .filter(|user| !user.is_empty())
        .any(|user| {
            text.match_indices(&format!("<@{user}"))
                .any(|(start, token)| {
                    let rest = &text[start + token.len()..];
                    rest.starts_with('>') || (rest.starts_with('|') && rest.contains('>'))
                })
        })
}

fn ignore_other_mentions(config: &Value, legacy: Option<&str>) -> bool {
    mention_flag(config, "ignore_other_user_mentions", legacy, false)
}

/// Mention settings intentionally do not strip string whitespace. Extras keep
/// Python truthiness for non-strings; the YAML bridge stringifies them first.
fn mention_flag(config: &Value, key: &str, legacy: Option<&str>, default: bool) -> bool {
    let extra = &config["platforms"]["slack"]["extra"][key];
    let value = if !extra.is_null() {
        extra.clone()
    } else if let Some(legacy) = legacy.filter(|s| !s.is_empty()) {
        json!(legacy)
    } else {
        let yaml = &config["slack"][key];
        if yaml.is_null() {
            return legacy
                .map(|s| {
                    if default {
                        !matches!(s.to_lowercase().as_str(), "false" | "0" | "no" | "off")
                    } else {
                        matches!(s.to_lowercase().as_str(), "true" | "1" | "yes" | "on")
                    }
                })
                .unwrap_or(default);
        }
        json!(yaml
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(yaml)))
    };
    match value.as_str() {
        Some(text) if default => {
            !matches!(text.to_lowercase().as_str(), "false" | "0" | "no" | "off")
        }
        Some(text) => matches!(text.to_lowercase().as_str(), "true" | "1" | "yes" | "on"),
        None => crate::python_value::truthy(&value),
    }
}

struct StrictMentionPolicy {
    require: bool,
    strict: bool,
    thread: bool,
    free_channels: std::collections::BTreeSet<String>,
    required_channels: std::collections::BTreeSet<String>,
}

/// Workspace-local Slack timestamps must not wake a different workspace. Empty
/// workspace markers retain the source's legacy, unscoped representation.
#[derive(Default)]
struct ThreadMarkers {
    bot: std::collections::BTreeSet<(String, String)>,
    mentioned: std::collections::BTreeSet<(String, String)>,
}

#[derive(Clone)]
#[allow(dead_code)]
struct ThreadContextEntry {
    content: String,
    parent_text: String,
    parent_user: String,
    messages: Vec<Value>,
    fetched: std::time::Instant,
}

#[allow(dead_code)]
struct ThreadRequest<'a> {
    channel: &'a str,
    thread: &'a str,
    current: &'a str,
    team: &'a str,
    after: &'a str,
    limit: usize,
    force: bool,
}

impl ThreadMarkers {
    fn remember_mention(&mut self, team: &str, timestamp: &str, cap: usize) {
        if timestamp.is_empty() {
            return;
        }
        self.mentioned.insert((team.into(), timestamp.into()));
        if self.mentioned.len() > cap {
            Self::discard_oldest(&mut self.mentioned, cap / 2);
        }
    }

    fn remember_sent(&mut self, team: &str, timestamp: &str, root: Option<&str>, cap: usize) {
        if timestamp.is_empty() {
            return;
        }
        self.bot.insert((team.into(), timestamp.into()));
        if let Some(root) = root.filter(|s| !s.is_empty()) {
            self.bot.insert((team.into(), root.into()));
        }
        if self.bot.len() > cap {
            let excess = self.bot.len() - cap / 2;
            Self::discard_oldest(&mut self.bot, excess);
        }
    }

    fn discard_oldest(markers: &mut std::collections::BTreeSet<(String, String)>, count: usize) {
        // Decimal components, not floating-point seconds, preserve microseconds
        // for long-lived timestamps. Normalize arbitrary-size Python integers
        // as signed digit strings rather than saturating them to machine ints.
        fn integer(raw: &str) -> (bool, String) {
            let Some(raw) = crate::python_value::numeric_text(raw) else {
                return (false, "0".into());
            };
            let negative = raw.starts_with('-');
            let digits = raw
                .strip_prefix('-')
                .or_else(|| raw.strip_prefix('+'))
                .unwrap_or(&raw);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return (false, "0".into());
            }
            let digits = digits.trim_start_matches('0');
            if digits.is_empty() {
                (false, "0".into())
            } else {
                (negative, digits.into())
            }
        }
        fn compare(a: &(bool, String), b: &(bool, String)) -> std::cmp::Ordering {
            if a.0 != b.0 {
                return b.0.cmp(&a.0);
            }
            let order = a.1.len().cmp(&b.1.len()).then_with(|| a.1.cmp(&b.1));
            if a.0 {
                order.reverse()
            } else {
                order
            }
        }
        let mut ordered: Vec<_> = markers
            .iter()
            .map(|marker| {
                let (seconds, fraction) = marker.1.split_once('.').unwrap_or((&marker.1, ""));
                let fraction: String = fraction
                    .chars()
                    .chain(std::iter::repeat('0'))
                    .take(6)
                    .collect();
                (marker.clone(), integer(seconds), integer(&fraction))
            })
            .collect();
        ordered.sort_by(|a, b| {
            compare(&a.1, &b.1)
                .then_with(|| compare(&a.2, &b.2))
                .then_with(|| a.0 .1.cmp(&b.0 .1))
        });
        for (marker, _, _) in ordered.into_iter().take(count) {
            markers.remove(&marker);
        }
    }
}

impl StrictMentionPolicy {
    fn from_config(config: &Value) -> Self {
        let list = |key: &str, env: &str, scalar: bool| {
            let mut extra = config["platforms"]["slack"]["extra"][key].clone();
            if !extra.is_null() && !extra.is_array() && !extra.is_string() {
                if !scalar {
                    return Default::default();
                }
                extra = json!(crate::python_value::python_repr(&extra));
            }
            let mapped = json!({"platforms":{"slack":{"extra":{"ignored_channels":extra}}},
                "slack":{"ignored_channels":config["slack"][key]}});
            ignored_channels(&mapped, std::env::var(env).ok().as_deref())
        };
        Self {
            require: mention_flag(
                config,
                "require_mention",
                std::env::var("SLACK_REQUIRE_MENTION").ok().as_deref(),
                true,
            ),
            strict: mention_flag(
                config,
                "strict_mention",
                std::env::var("SLACK_STRICT_MENTION").ok().as_deref(),
                false,
            ),
            thread: mention_flag(
                config,
                "thread_require_mention",
                std::env::var("SLACK_THREAD_REQUIRE_MENTION")
                    .ok()
                    .as_deref(),
                false,
            ),
            free_channels: list(
                "free_response_channels",
                "SLACK_FREE_RESPONSE_CHANNELS",
                true,
            ),
            required_channels: list(
                "require_mention_channels",
                "SLACK_REQUIRE_MENTION_CHANNELS",
                false,
            ),
        }
    }

    /// These are unconditional rejections before Python's asynchronous thread
    /// wake checks. Passing this predicate does not establish a wake reason.
    fn rejects(&self, event: &Value, mentioned: bool) -> bool {
        if mentioned || crate::python_value::truthy(&event["_hermes_force_process"]) {
            return false;
        }
        let channel = event["channel"].as_str().unwrap_or("");
        let free = !self.required_channels.contains(channel)
            && (self.free_channels.contains(channel) || !self.require);
        let reply =
            crate::python_value::truthy(&event["thread_ts"]) && event["thread_ts"] != event["ts"];
        (self.thread && reply) || (!free && self.strict)
    }
}

fn mention_patterns(config: &Value, legacy: Option<&str>) -> Vec<fancy_regex::Regex> {
    let extra = &config["platforms"]["slack"]["extra"]["mention_patterns"];
    let raw = if !extra.is_null() {
        extra.clone()
    } else {
        let raw = legacy
            .unwrap_or("")
            .trim_matches(crate::python_value::python_whitespace);
        serde_json::from_str(raw).unwrap_or_else(|_| {
            json!(raw
                .replace('\n', ",")
                .split(',')
                .map(|s| s.trim_matches(crate::python_value::python_whitespace))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>())
        })
    };
    crate::platform_helpers::compile_mention_patterns(
        Some(&raw),
        "Slack",
        Some("Slack"),
        None,
        None,
    )
}

/// Platform extras override the legacy environment bridge. A nonempty legacy
/// override wins over the top-level slack YAML block, as the plugin hook does.
fn ignored_channels(config: &Value, legacy: Option<&str>) -> std::collections::BTreeSet<String> {
    let extra = &config["platforms"]["slack"]["extra"]["ignored_channels"];
    let yaml = &config["slack"]["ignored_channels"];
    let text = |value: &Value| {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(value))
    };
    let raw = if !extra.is_null() {
        extra.clone()
    } else if let Some(value) = legacy.filter(|s| !s.is_empty()) {
        json!(value)
    } else if !yaml.is_null() {
        // Python's YAML hook joins list entries before the adapter parses the
        // comma-separated environment representation.
        json!(match yaml.as_array() {
            Some(values) => values.iter().map(text).collect::<Vec<_>>().join(","),
            None => text(yaml),
        })
    } else {
        json!(legacy.unwrap_or(""))
    };
    let values = match raw.as_array() {
        Some(values) => values.iter().map(text).collect::<Vec<_>>(),
        None => text(&raw).split(',').map(str::to_owned).collect(),
    };
    values
        .into_iter()
        .map(|s| {
            s.trim_matches(crate::python_value::python_whitespace)
                .to_owned()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn dedup_ttl(raw: Option<&str>) -> f64 {
    raw.and_then(crate::python_value::numeric_text)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(3600.0)
}

fn event_dedup_key(payload: &Value) -> String {
    let event = &payload["event"];
    let timestamp = event
        .get("_slack_changed_event_ts")
        .filter(|v| crate::python_value::truthy(v))
        .unwrap_or(&event["ts"]);
    if !crate::python_value::truthy(timestamp) {
        return String::new();
    }
    let text = |value: &Value| {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(value))
    };
    let team = event_team(payload);

    match team {
        Some(team) => format!("{}:{}", team, text(timestamp)),
        None => text(timestamp),
    }
}

fn event_team(payload: &Value) -> Option<String> {
    let event = &payload["event"];
    let team = [event, payload]
        .into_iter()
        .find_map(|object| {
            let team = object
                .get("team_id")
                .filter(|v| crate::python_value::truthy(v))
                .unwrap_or(&object["team"]);
            if team.as_str().is_some_and(|s| !s.is_empty()) {
                Some(team)
            } else {
                team.get("id").filter(|v| crate::python_value::truthy(v))
            }
        })
        .or_else(|| {
            payload["authorizations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.get("team_id"))
                .find(|value| crate::python_value::truthy(value))
        });
    team.map(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(value))
    })
}

/// Slack voice clips may be mislabeled as video. Genuine video files remain on
/// the video path; only Slack's stable voice markers override that MIME type.
/// Match the genuine-video branch after excluding Slack's video-labeled voice
/// clips. Preserve a supported filename suffix before consulting the MIME type.
fn video_extension(file: &Value) -> Option<&'static str> {
    let mime = file["mimetype"].as_str().unwrap_or("");
    if !mime.starts_with("video/") || audio_extension(file).is_some() {
        return None;
    }
    const TYPES: [(&str, &str); 5] = [
        (".mp4", "video/mp4"),
        (".mov", "video/quicktime"),
        (".webm", "video/webm"),
        (".mkv", "video/x-matroska"),
        (".avi", "video/x-msvideo"),
    ];
    let name = file["name"]
        .as_str()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    if let Some((stem, suffix)) = name.rsplit_once('.') {
        if stem.chars().any(|c| c != '.') {
            let ext = format!(".{}", suffix.to_lowercase());
            if let Some((ext, _)) = TYPES.iter().find(|(e, _)| *e == ext) {
                return Some(ext);
            }
        }
    }
    let mime = mime.split(';').next().unwrap_or("").to_lowercase();
    Some(
        TYPES
            .iter()
            .find(|(_, m)| *m == mime)
            .map_or(".mp4", |(e, _)| e),
    )
}

fn audio_extension(file: &Value) -> Option<String> {
    let mime = file["mimetype"].as_str().unwrap_or("unknown");
    let strip = |s: &str| {
        s.trim_matches(crate::python_value::python_whitespace)
            .to_lowercase()
    };
    let name = strip(file["name"].as_str().unwrap_or(""));
    let voice = strip(file["subtype"].as_str().unwrap_or("")) == "slack_audio"
        || name.starts_with("audio_message");
    if !(mime.starts_with("audio/") || mime.starts_with("video/") && voice) {
        return None;
    }
    let base = name.rsplit('/').next().unwrap_or("");
    if let Some((stem, suffix)) = base.rsplit_once('.') {
        if stem.chars().any(|c| c != '.')
            && matches!(
                suffix,
                "mp3" | "mp4" | "mpeg" | "mpga" | "m4a" | "wav" | "webm" | "ogg" | "aac" | "flac"
            )
        {
            return Some(format!(".{suffix}"));
        }
    }
    let mime = strip(mime.split(';').next().unwrap_or(""));
    Some(
        match mime.as_str() {
            "audio/ogg" | "audio/opus" => ".ogg",
            "audio/mpeg" | "audio/mp3" => ".mp3",
            "audio/wav" | "audio/x-wav" => ".wav",
            "audio/webm" => ".webm",
            "audio/flac" | "audio/x-flac" => ".flac",
            _ => ".m4a",
        }
        .into(),
    )
}

fn slack_file_url(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw)?;
    let host = url.host_str().unwrap_or("").trim_end_matches('.');
    anyhow::ensure!(
        url.scheme() == "https"
            && (matches!(host, "slack.com" | "slack-files.com")
                || host.ends_with(".slack.com")
                || host.ends_with(".slack-files.com"))
            && url.username().is_empty()
            && url.password().is_none(),
        "invalid Slack file destination"
    );
    Ok(url)
}

fn public_download_address(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    if let IpAddr::V6(v6) = ip {
        if let Some(v4) = v6.to_ipv4_mapped() {
            return public_download_address(IpAddr::V4(v4));
        }
    }
    if crate::local_probe::addr_is_private_or_loopback(ip)
        || ip.is_multicast()
        || ip.is_unspecified()
    {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => {
            u32::from(v4) >> 22 != u32::from(std::net::Ipv4Addr::new(100, 64, 0, 0)) >> 22
        }
        IpAddr::V6(v6) => v6.segments()[0] & 0xe000 == 0x2000,
    }
}

/// Slack Socket Mode adapter.
pub struct SlackAdapter {
    app_token: String,
    bot_token: String,
    user_bots: std::sync::Mutex<std::collections::VecDeque<((String, String), bool)>>,
    user_names: std::sync::Mutex<std::collections::VecDeque<((String, String), String)>>,
    reply_in_thread: bool,
    ignore_other_mentions: bool,
    mention_patterns: Vec<fancy_regex::Regex>,
    strict_mentions: StrictMentionPolicy,
    thread_markers: std::sync::Mutex<ThreadMarkers>,
    thread_context: std::sync::Mutex<std::collections::VecDeque<(String, ThreadContextEntry)>>,
    dm_thread_sessions: bool,
    disable_dms: bool,
    allowed_channels: std::collections::BTreeSet<String>,
    allow_bots: String,
    api_humans: std::collections::BTreeSet<String>,
    ignored_channels: std::collections::BTreeSet<String>,
    channel_teams: std::sync::Mutex<ChannelTeams>,
    bot_tokens: Vec<String>,
    team_tokens: std::sync::Mutex<WorkspaceCredentials>,
    api_base: String,
    client: reqwest::Client,
    audio_home: std::path::PathBuf,
    audio_limit: i64,
    dedup: std::sync::Mutex<crate::platform_helpers::MessageDeduplicator>,
}

/// Match Slack's Python media helpers: retry request/read timeouts and HTTP
/// status errors from 429 upward, but not access errors, HTML or cache failures.
async fn retry_media<F, Fut, T>(mut operation: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    for attempt in 0..3 {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                let retryable = error.downcast_ref::<reqwest::Error>().is_some_and(|e| {
                    e.is_timeout() || e.status().is_some_and(|s| s.as_u16() >= 429)
                });
                if !retryable || attempt == 2 {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(1500 * (attempt + 1))).await;
            }
        }
    }
    unreachable!("last attempt returns its error")
}

/// Resolve every hop separately and pin the resulting public addresses. Keep
/// URL hostnames intact for TLS verification; proxies must not re-resolve them.
async fn pinned_media_client(url: reqwest::Url) -> anyhow::Result<reqwest::Client> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("missing media host"))?;
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443))),
    )
    .await??
    .collect();
    anyhow::ensure!(
        !addresses.is_empty() && addresses.iter().all(|a| public_download_address(a.ip())),
        "unsafe Slack file address"
    );
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .build()?)
}

/// HTTPX strips Authorization on origin changes and never restores it later in
/// the chain. The initial URL is HTTPS Slack CDN; redirected public hosts may
/// serve signed media without receiving the bot token.
async fn follow_media_redirects<F, Fut>(
    mut url: reqwest::Url,
    token: String,
    cookies: &reqwest::cookie::Jar,
    mut client_for: F,
) -> anyhow::Result<reqwest::Response>
where
    F: FnMut(reqwest::Url) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<reqwest::Client>>,
{
    use reqwest::cookie::CookieStore;
    let mut authorized = true;
    for hop in 0..=20 {
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none(),
            "invalid media redirect destination"
        );
        let client = client_for(url.clone()).await?;
        let mut request = client.get(url.clone());
        if authorized {
            request = request.bearer_auth(&token);
        }
        if let Some(header) = cookies.cookies(&url) {
            request = request.header(reqwest::header::COOKIE, header);
        }
        let response = request.send().await?;
        cookies.set_cookies(
            &mut response
                .headers()
                .get_all(reqwest::header::SET_COOKIE)
                .iter(),
            &url,
        );
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
            return Ok(response);
        };
        anyhow::ensure!(hop < 20, "too many media redirects");
        let next = url.join(location.to_str()?)?;
        authorized &= url.origin() == next.origin();
        url = next;
    }
    unreachable!("redirect limit checked before advancing")
}

/// Token and bot identity are published together after authentication, so a
/// reconnect cannot pair a new token with a stale workspace bot user ID.
#[derive(Default)]
struct WorkspaceCredentials {
    teams: std::collections::HashMap<String, WorkspaceCredential>,
    primary_bot_user_id: String,
}

struct WorkspaceCredential {
    token: String,
    bot_user_id: String,
}

/// Python keeps separate insertion-ordered maps for observations and unambiguous
/// routes. Evict each independently, down to half capacity when it overflows.
#[derive(Default)]
struct ChannelTeams {
    observed: std::collections::VecDeque<(String, std::collections::BTreeSet<String>)>,
    routes: std::collections::VecDeque<(String, String)>,
}

impl ChannelTeams {
    fn remember(&mut self, channel: &str, team: &str, cap: usize) {
        if channel.is_empty() || team.is_empty() {
            return;
        }
        let index = self
            .observed
            .iter()
            .position(|(c, _)| c == channel)
            .unwrap_or_else(|| {
                self.observed
                    .push_back((channel.into(), Default::default()));
                self.observed.len() - 1
            });
        let teams = &mut self.observed[index].1;
        teams.insert(team.into());
        if teams.len() == 1 {
            if let Some((_, value)) = self.routes.iter_mut().find(|(c, _)| c == channel) {
                *value = team.into();
            } else {
                self.routes.push_back((channel.into(), team.into()));
            }
        } else {
            self.routes.retain(|(c, _)| c != channel);
        }
        if self.routes.len() > cap {
            self.routes.drain(..self.routes.len() - cap / 2);
        }
        if self.observed.len() > cap {
            self.observed.drain(..self.observed.len() - cap / 2);
        }
    }

    fn scope(&self, channel: &str) -> Option<String> {
        self.routes
            .iter()
            .find(|(c, _)| c == channel)
            .map(|(_, team)| team.clone())
    }
}

impl SlackAdapter {
    pub fn new(app_token: impl Into<String>, bot_token: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Other(format!("slack: build http client: {e}")))?;
        let raw: String = bot_token.into();
        let bot_tokens: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let primary = bot_tokens
            .first()
            .cloned()
            .ok_or_else(|| Error::Other("slack: no bot token configured".into()))?;
        Ok(Self {
            user_bots: Default::default(),
            user_names: Default::default(),
            reply_in_thread: true,
            strict_mentions: StrictMentionPolicy::from_config(&Value::Null),
            thread_markers: Default::default(),
            thread_context: Default::default(),
            ignore_other_mentions: ignore_other_mentions(
                &Value::Null,
                std::env::var("SLACK_IGNORE_OTHER_USER_MENTIONS")
                    .ok()
                    .as_deref(),
            ),
            mention_patterns: mention_patterns(
                &Value::Null,
                std::env::var("SLACK_MENTION_PATTERNS").ok().as_deref(),
            ),
            dm_thread_sessions: true,
            disable_dms: disable_dms(
                &Value::Null,
                std::env::var("SLACK_DISABLE_DMS").ok().as_deref(),
            ),
            allowed_channels: allowed_channels(
                &Value::Null,
                std::env::var("SLACK_ALLOWED_CHANNELS").ok().as_deref(),
            ),
            allow_bots: bot_policy(
                &Value::Null,
                std::env::var("SLACK_ALLOW_BOTS").ok().as_deref(),
            ),
            api_humans: Default::default(),
            ignored_channels: ignored_channels(
                &Value::Null,
                std::env::var("SLACK_IGNORED_CHANNELS").ok().as_deref(),
            ),
            channel_teams: std::sync::Mutex::new(ChannelTeams::default()),
            bot_tokens,
            team_tokens: std::sync::Mutex::new(WorkspaceCredentials::default()),
            app_token: app_token.into(),
            bot_token: primary,
            api_base: API_BASE.to_string(),
            client,
            audio_home: crate::config_file::hermes_home(),
            audio_limit: 128 * 1024 * 1024,
            dedup: std::sync::Mutex::new(crate::platform_helpers::MessageDeduplicator::new(
                2000,
                dedup_ttl(std::env::var("SLACK_DEDUP_TTL_SECONDS").ok().as_deref()),
            )),
        })
    }

    fn is_ignored_channel(&self, channel: &str) -> bool {
        !channel.is_empty()
            && (self.ignored_channels.contains("*")
                || self
                    .ignored_channels
                    .contains(channel.split(':').next().unwrap_or("")))
    }

    fn resolve_event_workspace(&self, payload: &Value) -> Value {
        let event = &payload["event"];
        let channel = event["channel"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| event["channel_id"].as_str())
            .unwrap_or("");
        let mut cache = self.channel_teams.lock().unwrap();
        let explicit = event_team(payload);
        let team = explicit.clone().or_else(|| cache.scope(channel));
        if let Some(team) = &explicit {
            cache.remember(channel, team, 10000);
        }
        let mut resolved = payload.clone();
        if explicit.is_none() {
            if let Some(team) = team {
                resolved["event"]["team_id"] = json!(team);
            }
        }
        resolved
    }

    /// Resolve token identities again on each connection cycle. Build the map
    /// privately so a failed authentication cannot publish a partial rotation.
    async fn register_workspaces(&self) -> anyhow::Result<()> {
        let mut tokens = self.bot_tokens.clone();
        match tokio::fs::read(self.audio_home.join("slack_tokens.json")).await {
            Ok(bytes) => {
                if let Ok(Value::Object(saved)) = serde_json::from_slice::<Value>(&bytes) {
                    for entry in saved.values() {
                        if let Some(token) = entry["token"].as_str().filter(|s| !s.is_empty()) {
                            if !tokens.iter().any(|t| t == token) {
                                tokens.push(token.into());
                            }
                        }
                    }
                } else {
                    warn!("slack saved workspace tokens could not be decoded");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => warn!("slack saved workspace tokens could not be read"),
        }
        let mut teams = std::collections::HashMap::new();
        let mut primary_bot_user_id = None;
        for token in tokens {
            let response = self
                .client
                .post(format!("{}/auth.test", self.api_base))
                .bearer_auth(&token)
                .send()
                .await?;
            anyhow::ensure!(
                response.status().is_success(),
                "Slack workspace authentication failed"
            );
            let response: Value = response.json().await?;
            anyhow::ensure!(
                response["ok"] == true,
                "Slack workspace authentication failed"
            );
            let team = response["team_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("Slack authentication omitted workspace identity")
                })?;
            primary_bot_user_id
                .get_or_insert_with(|| response["user_id"].as_str().unwrap_or("").to_owned());
            teams.insert(
                team.into(),
                WorkspaceCredential {
                    token,
                    bot_user_id: response["user_id"].as_str().unwrap_or("").into(),
                },
            );
        }
        *self.team_tokens.lock().unwrap() = WorkspaceCredentials {
            teams,
            primary_bot_user_id: primary_bot_user_id.unwrap_or_default(),
        };
        Ok(())
    }

    fn workspace_token(&self, team: &str, url: &str) -> String {
        let teams = self.team_tokens.lock().unwrap();
        if let Some(token) = teams.teams.get(team) {
            return token.token.clone();
        }
        // Slack embeds the owning workspace in private-file URLs. Match the
        // same uppercase T[A-Z0-9]+ prefix as Python's download resolver.
        for (offset, marker) in url.match_indices("/files-pri/") {
            let start = offset + marker.len();
            if let Some((candidate, _)) = url[start..].split_once('-') {
                if candidate.starts_with('T')
                    && candidate.len() > 1
                    && candidate
                        .bytes()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                {
                    if let Some(token) = teams.teams.get(candidate) {
                        return token.token.clone();
                    }
                }
            }
        }
        self.bot_token.clone()
    }

    fn is_self_sender(&self, team: Option<&str>, user: &str) -> bool {
        if user.is_empty() {
            return false;
        }
        let teams = self.team_tokens.lock().unwrap();
        let bot = team
            .and_then(|team| teams.teams.get(team))
            .map(|identity| identity.bot_user_id.as_str())
            .unwrap_or(&teams.primary_bot_user_id);
        !bot.is_empty() && bot == user
    }

    /// Cache both bot and human classifications by workspace. Lookup failures
    /// retain Python's permissive false result; no credential-bearing error is logged.
    async fn resolve_user_is_bot(&self, team: &str, user: &str) -> bool {
        if user.is_empty() {
            return false;
        }
        let key = (team.to_owned(), user.to_owned());
        if let Some((_, result)) = self
            .user_bots
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k == &key)
        {
            return *result;
        }
        // Runtime registers authenticated workspaces before consuming events.
        // An unconnected adapter has no user-directory client, as in Python.
        if self.team_tokens.lock().unwrap().teams.is_empty() {
            let mut cache = self.user_bots.lock().unwrap();
            if let Some((_, old)) = cache.iter_mut().find(|(k, _)| k == &key) {
                *old = false;
            } else {
                cache.push_back((key, false));
            }
            return false;
        }
        let result: anyhow::Result<bool> = async {
            let response = self
                .client
                .get(format!("{}/users.info", self.api_base))
                .bearer_auth(self.workspace_token(team, ""))
                .query(&[("user", user)])
                .send()
                .await?;
            anyhow::ensure!(response.status().is_success(), "Slack user lookup failed");
            let payload: Value = response.json().await?;
            anyhow::ensure!(payload["ok"] == true, "Slack user lookup failed");
            let user = &payload["user"];
            anyhow::ensure!(
                user.is_object() && user.get("profile").is_none_or(Value::is_object),
                "Slack user profile malformed"
            );
            if user.is_object() && user.get("profile").is_none_or(Value::is_object) {
                let name = [
                    &user["profile"]["display_name"],
                    &user["profile"]["real_name"],
                    &user["real_name"],
                    &user["name"],
                ]
                .into_iter()
                .find(|v| crate::python_value::truthy(v));
                let name = name
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| crate::python_value::python_repr(v))
                    })
                    .unwrap_or_else(|| key.1.clone());
                let mut cache = self.user_names.lock().unwrap();
                if let Some((_, old)) = cache.iter_mut().find(|(k, _)| k == &key) {
                    *old = name;
                } else {
                    cache.push_back((key.clone(), name));
                }
            }
            Ok(crate::python_value::truthy(&user["is_bot"])
                || crate::python_value::truthy(&user["is_workflow_bot"])
                || crate::python_value::truthy(&user["profile"]["bot_id"]))
        }
        .await;
        let success = result.is_ok();
        let result = result.unwrap_or(false);
        let mut cache = self.user_bots.lock().unwrap();
        if let Some((_, old)) = cache.iter_mut().find(|(k, _)| k == &key) {
            *old = result;
        } else {
            cache.push_back((key, result));
        }
        if success && cache.len() > 5000 {
            let excess = cache.len() - 2500;
            cache.drain(..excess);
        }
        result
    }

    /// Name lookup also seeds bot classification, as in the source. Failures
    /// cache the raw user ID without overwriting an existing bot classification.
    #[allow(dead_code)]
    async fn resolve_user_name(&self, team: &str, channel: &str, user: &str) -> String {
        if user.is_empty() {
            return String::new();
        }
        let team = if team.is_empty() {
            self.channel_teams
                .lock()
                .unwrap()
                .scope(channel)
                .unwrap_or_default()
        } else {
            team.into()
        };
        let key = (team.clone(), user.to_owned());
        if let Some((_, name)) = self
            .user_names
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k == &key)
        {
            return name.clone();
        }
        if self.team_tokens.lock().unwrap().teams.is_empty() {
            return user.into();
        }
        let result: anyhow::Result<String> = async {
            // A channel-less source lookup uses the primary app client even
            // when the caller supplied a workspace for the cache key.
            let token = if channel.is_empty() {
                self.bot_token.clone()
            } else {
                self.workspace_token(&team, "")
            };
            let response = self
                .client
                .get(format!("{}/users.info", self.api_base))
                .bearer_auth(token)
                .query(&[("user", user)])
                .send()
                .await?;
            anyhow::ensure!(response.status().is_success(), "Slack name lookup failed");
            let payload: Value = response.json().await?;
            anyhow::ensure!(payload["ok"] == true, "Slack name unavailable");
            let info = &payload["user"];
            anyhow::ensure!(info.is_object(), "Slack user malformed");
            let profile = info.get("profile").cloned().unwrap_or_else(|| json!({}));
            let bot = crate::python_value::truthy(&info["is_bot"])
                || crate::python_value::truthy(&info["is_workflow_bot"])
                || crate::python_value::truthy(&profile["bot_id"]);
            {
                let mut cache = self.user_bots.lock().unwrap();
                if let Some((_, old)) = cache.iter_mut().find(|(k, _)| k == &key) {
                    *old = bot;
                } else {
                    cache.push_back((key.clone(), bot));
                }
            }
            anyhow::ensure!(profile.is_object(), "Slack profile malformed");
            let name = [
                &profile["display_name"],
                &profile["real_name"],
                &info["real_name"],
                &info["name"],
            ]
            .into_iter()
            .find(|v| crate::python_value::truthy(v));
            Ok(name
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| crate::python_value::python_repr(v))
                })
                .unwrap_or_else(|| user.into()))
        }
        .await;
        let name = result.unwrap_or_else(|_| user.into());
        let mut cache = self.user_names.lock().unwrap();
        if let Some((_, old)) = cache.iter_mut().find(|(k, _)| k == &key) {
            *old = name.clone();
        } else {
            cache.push_back((key, name.clone()));
        }
        if cache.len() > 5000 {
            let excess = cache.len() - 2500;
            cache.drain(..excess);
        }
        name
    }

    fn session_thread(&self, event: &Value) -> Option<String> {
        let ts = event["ts"].as_str().filter(|s| !s.is_empty());
        let thread = event["thread_ts"].as_str().filter(|s| !s.is_empty());
        let selected = if matches!(channel_kind(event), "im" | "mpim") {
            thread.or(if self.dm_thread_sessions { ts } else { None })
        } else if crate::python_value::truthy(&event["_hermes_no_thread_response"])
            || (thread.is_some() && thread != ts)
        {
            thread
        } else if self.reply_in_thread {
            ts
        } else {
            None
        };
        selected.map(str::to_owned)
    }

    fn reply_thread<'a>(&self, message: &'a Message) -> Option<&'a str> {
        let thread = message.thread_id.as_deref().filter(|s| !s.is_empty());
        let reply_to = message.message_id.as_deref().filter(|s| !s.is_empty());
        if self.reply_in_thread {
            thread.or(reply_to)
        } else {
            thread.filter(|t| Some(*t) != reply_to)
        }
    }

    fn permits_channel(&self, event: &Value, team: Option<&str>) -> bool {
        let kind = channel_kind(event);
        if matches!(kind, "im" | "mpim") && self.disable_dms {
            return false;
        }
        if kind == "im" || self.allowed_channels.is_empty() {
            return true;
        }
        let registry = self.team_tokens.lock().unwrap();
        let bot = team
            .and_then(|t| registry.teams.get(t))
            .map(|c| c.bot_user_id.as_str())
            .unwrap_or(&registry.primary_bot_user_id);
        // Source channel controls apply only once the bot identity is known.
        bot.is_empty()
            || self
                .allowed_channels
                .contains(event["channel"].as_str().unwrap_or(""))
    }

    fn permits_bot(&self, event: &Value, team: Option<&str>, primary_gate: bool) -> bool {
        match self.allow_bots.as_str() {
            "all" => true,
            "mentions" => {
                let registry = self.team_tokens.lock().unwrap();
                let primary = registry.primary_bot_user_id.as_str();
                let scoped = team
                    .and_then(|t| registry.teams.get(t))
                    .map(|c| c.bot_user_id.as_str())
                    .unwrap_or(primary);
                (!primary_gate || primary.is_empty() || mentions_user(event, primary))
                    && (scoped.is_empty()
                        || mentions_user(event, scoped)
                        || self.mention_patterns.iter().any(|pattern| {
                            pattern
                                .is_match(&mention_detection_text(event))
                                .unwrap_or(false)
                        }))
            }
            _ => false,
        }
    }

    pub fn with_audio_cache(mut self, home: std::path::PathBuf, config: &Value) -> Self {
        self.audio_home = home;
        self.strict_mentions = StrictMentionPolicy::from_config(config);
        self.ignore_other_mentions = ignore_other_mentions(
            config,
            std::env::var("SLACK_IGNORE_OTHER_USER_MENTIONS")
                .ok()
                .as_deref(),
        );
        self.mention_patterns = mention_patterns(
            config,
            std::env::var("SLACK_MENTION_PATTERNS").ok().as_deref(),
        );
        let extra = &config["platforms"]["slack"]["extra"];
        self.reply_in_thread = extra
            .get("reply_in_thread")
            .map(crate::python_value::truthy)
            .unwrap_or(true);
        self.dm_thread_sessions = match extra
            .get("dm_top_level_threads_as_sessions")
            .filter(|v| !v.is_null())
        {
            None => true,
            Some(value) => matches!(
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::python_value::python_repr(value))
                    .trim_matches(crate::python_value::python_whitespace)
                    .to_lowercase()
                    .as_str(),
                "1" | "true" | "yes" | "on"
            ),
        };
        self.disable_dms = disable_dms(config, std::env::var("SLACK_DISABLE_DMS").ok().as_deref());
        self.allowed_channels = allowed_channels(
            config,
            std::env::var("SLACK_ALLOWED_CHANNELS").ok().as_deref(),
        );
        self.allow_bots = bot_policy(config, std::env::var("SLACK_ALLOW_BOTS").ok().as_deref());
        // This allowlist shares the extras-list / environment-CSV parsing rules.
        let human_config = json!({"platforms":{"slack":{"extra":{"ignored_channels":config["platforms"]["slack"]["extra"]["api_human_users"]}}}});
        self.api_humans = ignored_channels(
            &human_config,
            std::env::var("SLACK_API_HUMAN_USERS").ok().as_deref(),
        );
        self.ignored_channels = ignored_channels(
            config,
            std::env::var("SLACK_IGNORED_CHANNELS").ok().as_deref(),
        );
        self.audio_limit = crate::audio_process::inbound_limit(config);
        self
    }

    async fn download_attachment(
        &self,
        team: &str,
        raw: &str,
        extension: &str,
        video: bool,
    ) -> anyhow::Result<std::path::PathBuf> {
        let url = slack_file_url(raw)?;
        let token = self.workspace_token(team, url.as_str());
        // Scope cookies to this attachment while retaining them across retries.
        let cookies = reqwest::cookie::Jar::default();
        retry_media(|| async {
            let response =
                follow_media_redirects(url.clone(), token.clone(), &cookies, pinned_media_client)
                    .await?;
            self.cache_attachment(response, extension, video).await
        })
        .await
    }

    #[cfg(test)]
    async fn fetch_attachment(
        &self,
        team: &str,
        client: &reqwest::Client,
        url: reqwest::Url,
        extension: &str,
        video: bool,
    ) -> anyhow::Result<std::path::PathBuf> {
        let token = self.workspace_token(team, url.as_str());
        let response = client.get(url).bearer_auth(token).send().await?;
        self.cache_attachment(response, extension, video).await
    }

    async fn cache_attachment(
        &self,
        response: reqwest::Response,
        extension: &str,
        video: bool,
    ) -> anyhow::Result<std::path::PathBuf> {
        // Status classification precedes HTML detection, as in Python. Error
        // pages for retryable statuses must still enter the retry path.
        let response = response.error_for_status()?;
        anyhow::ensure!(
            !response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .contains("text/html"),
            "Slack returned HTML instead of media"
        );
        if !video {
            return crate::audio_process::cache_audio_response(
                &self.audio_home,
                response,
                extension,
                self.audio_limit,
            )
            .await;
        }
        // MP4 video must retain its video suffix, not the audio sniffer's M4A.
        anyhow::ensure!(
            [".mp4", ".mov", ".webm", ".mkv", ".avi"].contains(&extension),
            "invalid video extension"
        );
        let bytes = crate::audio_process::read_inbound_response(response, self.audio_limit).await?;
        let directory = crate::config_file::get_hermes_dir(
            "cache/videos",
            "video_cache",
            Some(&self.audio_home),
        );
        tokio::fs::create_dir_all(&directory).await?;
        let id = crate::install_identity::mint_id()
            .ok_or_else(|| anyhow::anyhow!("video cache identity unavailable"))?;
        let path = directory.join(format!("video_{}{extension}", &id[..12]));
        tokio::fs::write(&path, bytes).await?;
        Ok(path)
    }

    /// Both Connect stubs and lifecycle events resolve through the same token
    /// and response validation policy.
    async fn file_info(&self, team: &str, id: &str) -> anyhow::Result<Value> {
        let response = self
            .client
            .get(format!("{}/files.info", self.api_base))
            .bearer_auth(self.workspace_token(team, ""))
            .query(&[("file", id)])
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "Slack file metadata request failed"
        );
        let response: Value = response.json().await?;
        anyhow::ensure!(
            response["ok"] == true && response["file"].is_object(),
            "Slack file metadata unavailable"
        );
        Ok(response["file"].clone())
    }

    #[allow(dead_code)]
    async fn format_thread_messages(
        &self,
        messages: &[Value],
        request: &ThreadRequest<'_>,
        after: &str,
        authorized: &(dyn Fn(&str) -> Option<bool> + Sync),
    ) -> (String, String) {
        let (primary, bots) = {
            let registry = self.team_tokens.lock().unwrap();
            (
                registry.primary_bot_user_id.clone(),
                registry
                    .teams
                    .iter()
                    .map(|(team, entry)| (team.clone(), entry.bot_user_id.clone()))
                    .collect::<std::collections::HashMap<_, _>>(),
            )
        };
        let bot = bots.get(request.team).unwrap_or(&primary);
        let mut names = std::collections::HashMap::new();
        // Resolve only names the formatter will use. Current and consumed
        // messages, empty bodies and own assistant replies need no lookup.
        for message in messages {
            let ts = message["ts"].as_str().unwrap_or("");
            if ts == request.current || (!after.is_empty() && !ts.is_empty() && ts <= after) {
                continue;
            }
            let parent = ts == request.thread;
            let user = message["user"].as_str().unwrap_or("");
            let team = message["team"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(request.team);
            let own = bots
                .get(team)
                .filter(|s| !team.is_empty() && !s.is_empty())
                .unwrap_or(&primary);
            if !parent && !own.is_empty() && own == user && declares_bot(message, &self.api_humans)
            {
                continue;
            }
            if crate::slack_blocks::render_message(message, bot).is_empty() {
                continue;
            }
            let user = if user.is_empty() { "unknown" } else { user };
            names.insert(
                user.to_owned(),
                self.resolve_user_name(request.team, request.channel, user)
                    .await,
            );
        }
        let declared = |message: &Value| declares_bot(message, &self.api_humans);
        let name = |user: &str| names.get(user).cloned().unwrap_or_else(|| user.into());
        crate::slack_blocks::format_thread(
            messages,
            &crate::slack_blocks::ThreadFormat {
                thread: request.thread,
                current: request.current,
                after,
                bot,
                primary_bot: &primary,
                team: request.team,
                team_bots: &bots,
                declared_bot: &declared,
                name: &name,
                authorized,
            },
        )
    }

    /// Fetch full context once, retaining raw messages for watermark deltas.
    /// The caller supplies the runner's authorization decision for each sender.
    #[allow(dead_code)]
    async fn fetch_thread_context(
        &self,
        request: &ThreadRequest<'_>,
        authorized: &(dyn Fn(&str) -> Option<bool> + Sync),
    ) -> String {
        let key = format!("{}:{}:{}", request.channel, request.thread, request.team);
        let now = std::time::Instant::now();
        let cached = if request.force {
            None
        } else {
            self.thread_context
                .lock()
                .unwrap()
                .iter()
                .find(|(k, _)| k == &key)
                .map(|(_, entry)| entry.clone())
        };
        if let Some(cached) =
            cached.filter(|entry| now.duration_since(entry.fetched) < Duration::from_secs(60))
        {
            if request.after.is_empty() || cached.messages.is_empty() {
                return cached.content;
            }
            return self
                .format_thread_messages(&cached.messages, request, request.after, authorized)
                .await
                .0;
        }
        let result: anyhow::Result<Vec<Value>> = async {
            for attempt in 0..3 {
                let response = self
                    .client
                    .get(format!("{}/conversations.replies", self.api_base))
                    .bearer_auth(self.workspace_token(request.team, ""))
                    .query(&[
                        ("channel", request.channel.to_owned()),
                        ("ts", request.thread.to_owned()),
                        ("limit", request.limit.saturating_add(1).to_string()),
                        ("inclusive", "true".into()),
                    ])
                    .send()
                    .await?;
                let status = response.status();
                let payload: Value = response.json().await.unwrap_or(Value::Null);
                let error = payload["error"].as_str().unwrap_or("").to_lowercase();
                let rate_limited = status.as_u16() == 429
                    || error.contains("429")
                    || error.contains("ratelimited")
                    || error.contains("rate_limited");
                if rate_limited && attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    continue;
                }
                anyhow::ensure!(
                    status.is_success() && payload["ok"] == true,
                    "Slack thread lookup failed"
                );
                return Ok(payload["messages"].as_array().cloned().unwrap_or_default());
            }
            unreachable!()
        }
        .await;
        let Ok(messages) = result else {
            return String::new();
        };
        if messages.is_empty() {
            return String::new();
        }
        let (content, parent_text) = self
            .format_thread_messages(&messages, request, "", authorized)
            .await;
        let parent_user = messages
            .iter()
            .find(|m| m["ts"].as_str() == Some(request.thread))
            .and_then(|m| m["user"].as_str())
            .unwrap_or("")
            .to_owned();
        {
            let mut cache = self.thread_context.lock().unwrap();
            let entry = ThreadContextEntry {
                content: content.clone(),
                parent_text,
                parent_user,
                messages: messages.clone(),
                fetched: now,
            };
            if let Some((_, old)) = cache.iter_mut().find(|(k, _)| k == &key) {
                *old = entry;
            } else {
                cache.push_back((key, entry));
            }
            if cache.len() > 2500 {
                cache.retain(|(_, entry)| {
                    now.duration_since(entry.fetched) < Duration::from_secs(60)
                });
            }
        }
        if request.after.is_empty() {
            content
        } else {
            self.format_thread_messages(&messages, request, request.after, authorized)
                .await
                .0
        }
    }

    /// Source root-authorship check deliberately uses the first cache key with
    /// this channel/thread prefix, even if stale or stored under another team.
    #[allow(dead_code)]
    async fn bot_authored_thread_root(
        &self,
        request: &ThreadRequest<'_>,
        authorized: &(dyn Fn(&str) -> Option<bool> + Sync),
    ) -> bool {
        if request.thread.is_empty() {
            return false;
        }
        let bot = {
            let registry = self.team_tokens.lock().unwrap();
            registry
                .teams
                .get(request.team)
                .map(|entry| entry.bot_user_id.clone())
                .unwrap_or_else(|| registry.primary_bot_user_id.clone())
        };
        if bot.is_empty() {
            return false;
        }
        let prefix = format!("{}:{}:", request.channel, request.thread);
        let cached = || {
            self.thread_context
                .lock()
                .unwrap()
                .iter()
                .find(|(key, _)| key.starts_with(&prefix))
                .map(|(_, entry)| !entry.parent_user.is_empty() && entry.parent_user == bot)
        };
        if let Some(found) = cached() {
            return found;
        }
        let fetch = ThreadRequest {
            channel: request.channel,
            thread: request.thread,
            team: request.team,
            current: "",
            after: "",
            limit: 30,
            force: false,
        };
        self.fetch_thread_context(&fetch, authorized).await;
        cached().unwrap_or(false)
    }

    /// Combine the source's five wake checks. Session activity must come from
    /// a reset-aware store query supplied by the caller, never history presence.
    #[allow(dead_code)]
    async fn should_wake_on_unmentioned(
        &self,
        request: &ThreadRequest<'_>,
        active_session: &(dyn Fn() -> bool + Sync),
        authorized: &(dyn Fn(&str) -> Option<bool> + Sync),
    ) -> bool {
        if request.thread.is_empty() {
            return false;
        }
        let reply = request.thread != request.current;
        {
            let markers = self.thread_markers.lock().unwrap();
            let scoped = (request.team.to_owned(), request.thread.to_owned());
            let legacy = (String::new(), request.thread.to_owned());
            if reply && (markers.bot.contains(&scoped) || markers.bot.contains(&legacy)) {
                return true;
            }
            if markers.mentioned.contains(&scoped) || markers.mentioned.contains(&legacy) {
                return true;
            }
        }
        if !reply {
            return false;
        }
        if active_session() {
            return true;
        }
        if self.bot_authored_thread_root(request, authorized).await {
            return true;
        }
        let bot = {
            let registry = self.team_tokens.lock().unwrap();
            registry
                .teams
                .get(request.team)
                .map(|entry| entry.bot_user_id.clone())
                .unwrap_or_else(|| registry.primary_bot_user_id.clone())
        };
        if !bot.is_empty() {
            let parent = self
                .fetch_thread_parent_text(request.channel, request.thread, request.team, false)
                .await;
            if parent.contains(&format!("<@{bot}>")) {
                if !self.strict_mentions.strict {
                    // This particular source call omits team_id. Preserve its
                    // legacy marker so later replies see the same fallback.
                    self.thread_markers
                        .lock()
                        .unwrap()
                        .remember_mention("", request.thread, 5000);
                }
                return true;
            }
        }
        false
    }

    /// Cold-cache branch of the source parent lookup. The thread-context cache
    /// and wake resolver will call this after their local evidence checks.
    #[allow(dead_code)]
    async fn fetch_thread_parent_text(
        &self,
        channel: &str,
        thread: &str,
        team: &str,
        strip_bot_mention: bool,
    ) -> String {
        let key = format!("{channel}:{thread}:{team}");
        let cached = self
            .thread_context
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k == &key)
            .map(|(_, entry)| entry.clone());
        if let Some(cached) =
            cached.filter(|entry| entry.fetched.elapsed() < Duration::from_secs(60))
        {
            if strip_bot_mention {
                return cached.parent_text;
            }
            if let Some(parent) = cached
                .messages
                .iter()
                .find(|m| m["ts"].as_str() == Some(thread))
            {
                return parent["text"]
                    .as_str()
                    .unwrap_or("")
                    .trim_matches(crate::python_value::python_whitespace)
                    .to_owned();
            }
        }
        let result: anyhow::Result<String> = async {
            let response = self
                .client
                .get(format!("{}/conversations.replies", self.api_base))
                .bearer_auth(self.workspace_token(team, ""))
                .query(&[
                    ("channel", channel),
                    ("ts", thread),
                    ("limit", "1"),
                    ("inclusive", "true"),
                ])
                .send()
                .await?;
            anyhow::ensure!(response.status().is_success(), "Slack parent lookup failed");
            let payload: Value = response.json().await?;
            anyhow::ensure!(payload["ok"] == true, "Slack parent unavailable");
            let Some(parent) = payload["messages"]
                .as_array()
                .and_then(|messages| messages.first())
            else {
                return Ok(String::new());
            };
            if parent["ts"].as_str() != Some(thread) {
                return Ok(String::new());
            }
            let bot = {
                let registry = self.team_tokens.lock().unwrap();
                registry
                    .teams
                    .get(team)
                    .map(|c| c.bot_user_id.clone())
                    .unwrap_or_else(|| registry.primary_bot_user_id.clone())
            };
            // The source renderer itself strips mentions even when the caller
            // asks to preserve them. Keep that cold-cache behavior until the
            // raw-message cache path is ported; do not invent different text.
            let mut text = crate::slack_blocks::render_message(parent, &bot);
            if strip_bot_mention && !bot.is_empty() {
                text = text
                    .replace(&format!("<@{bot}>"), "")
                    .trim_matches(crate::python_value::python_whitespace)
                    .to_owned();
            }
            Ok(text)
        }
        .await;
        result.unwrap_or_default()
    }

    async fn prepare_event(&self, payload: Value) -> Option<Message> {
        if payload["event"]["type"] != "file_shared" {
            return self.prepare_message(&payload).await;
        }
        let channel = payload["event"]["channel_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| payload["event"]["channel"].as_str())
            .unwrap_or("");
        if self.is_ignored_channel(channel) {
            return None;
        }
        let mut claim_payload = payload.clone();
        let payload = self.resolve_event_workspace(&payload);
        let event = &payload["event"];
        let channel = event["channel_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| event["channel"].as_str())
            .filter(|s| !s.is_empty())?;
        let id = event["file_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| event["file"]["id"].as_str())
            .filter(|s| !s.is_empty())?;
        let file = match self
            .file_info(event_team(&payload).as_deref().unwrap_or(""), id)
            .await
        {
            Ok(file) => file,
            Err(_) => {
                warn!("slack lifecycle file metadata lookup failed");
                return None;
            }
        };
        // Python intentionally limits this fallback to video MIME types,
        // including voice clips that Slack happens to label as video.
        if !file["mimetype"]
            .as_str()
            .is_some_and(|s| s.starts_with("video/"))
        {
            return None;
        }
        let mut share = None;
        for bucket in file["shares"]
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
        {
            let Some(bucket) = bucket.as_object() else {
                continue;
            };
            if let Some(first) = bucket
                .get(channel)
                .and_then(Value::as_array)
                .and_then(|a| a.first())
            {
                share = Some(first);
                break;
            }
            if share.is_none() {
                share = bucket
                    .values()
                    .filter_map(Value::as_array)
                    .find_map(|a| a.first());
            }
        }
        let timestamp = share
            .and_then(|s| s.get("ts"))
            .filter(|v| crate::python_value::truthy(v))
            .unwrap_or(&event["event_ts"])
            .clone();
        let mut fallback = payload.clone();
        // Keep workspace metadata in the event/body so normal messages and the
        // fallback claim exactly the same workspace-scoped share timestamp.
        fallback["event"]["type"] = json!("message");
        fallback["event"]["subtype"] = json!("file_share");
        fallback["event"]["channel"] = json!(channel);
        fallback["event"]["channel_type"] = json!(if channel.starts_with('D') {
            "im"
        } else {
            "channel"
        });
        fallback["event"]["user"] = event["user_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| file["user"].as_str())
            .unwrap_or("")
            .into();
        claim_payload["event"]["ts"] = timestamp.clone();
        fallback["event"]["ts"] = timestamp;
        fallback["event"]["text"] = json!("");
        if let Some(thread) = share.and_then(|s| s.get("thread_ts")) {
            if crate::python_value::truthy(thread) && thread != &fallback["event"]["ts"] {
                fallback["event"]["thread_ts"] = thread.clone();
            }
        }
        fallback["event"]["files"] = json!([file]);
        tokio::time::sleep(Duration::from_millis(750)).await;
        if self
            .dedup
            .lock()
            .unwrap()
            .is_duplicate(&event_dedup_key(&claim_payload))
        {
            return None;
        }
        // Lifecycle shares claim the original workspace identity themselves.
        fallback["event"]["ts"] = json!("");
        self.prepare_message(&fallback).await
    }

    async fn resolve_message_files(&self, payload: &Value) -> Option<Value> {
        if !eligible_message(&payload["event"]) {
            return None;
        }
        let mut resolved = payload.clone();
        let Some(files) = payload["event"]["files"].as_array() else {
            return Some(resolved);
        };
        let mut ready = Vec::new();
        let mut failures = 0;
        for file in files {
            if file["file_access"] != "check_file_info" {
                ready.push(file.clone());
                continue;
            }
            let Some(id) = file["id"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let result = self
                .file_info(event_team(payload).as_deref().unwrap_or(""), id)
                .await;
            match result {
                Ok(file) => ready.push(file),
                Err(_) => {
                    failures += 1;
                    warn!("slack file metadata lookup failed");
                }
            }
        }
        resolved["event"]["files"] = Value::Array(ready);
        if failures > 0 {
            let mut text = payload["event"]["text"].as_str().unwrap_or("").to_owned();
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("[File attachment metadata could not be retrieved.]");
            resolved["event"]["text"] = json!(text);
        }
        Some(resolved)
    }

    async fn prepare_message(&self, payload: &Value) -> Option<Message> {
        if !eligible_message(&payload["event"]) {
            return None;
        }
        // The claim is atomic and ends before any await. Keep it on the adapter
        // across reconnects, and scope timestamps by workspace, as Python does.
        if self
            .dedup
            .lock()
            .unwrap()
            .is_duplicate(&event_dedup_key(payload))
        {
            return None;
        }
        if self.is_ignored_channel(payload["event"]["channel"].as_str().unwrap_or("")) {
            return None;
        }
        let payload = self.resolve_event_workspace(payload);
        if !self.permits_channel(&payload["event"], event_team(&payload).as_deref()) {
            return None;
        }
        // Some Socket Mode self posts look like ordinary user messages. Use
        // authenticated identity before metadata or media work, not bot_id alone.
        if self.is_self_sender(
            event_team(&payload).as_deref(),
            payload["event"]["user"].as_str().unwrap_or(""),
        ) {
            return None;
        }
        let team = event_team(&payload);
        let event = &payload["event"];
        let sender_is_bot = declares_bot(event, &self.api_humans)
            || self
                .resolve_user_is_bot(
                    team.as_deref().unwrap_or(""),
                    event["user"].as_str().unwrap_or(""),
                )
                .await;
        let primary_gate = declares_bot(event, &self.api_humans)
            || !crate::python_value::truthy(&event["client_msg_id"]);
        if sender_is_bot && !self.permits_bot(event, team.as_deref(), primary_gate) {
            return None;
        }
        let (bot, primary) = {
            let registry = self.team_tokens.lock().unwrap();
            let bot = team
                .as_deref()
                .and_then(|t| registry.teams.get(t))
                .map(|c| c.bot_user_id.clone())
                .unwrap_or_else(|| registry.primary_bot_user_id.clone());
            (bot, registry.primary_bot_user_id.clone())
        };
        let routing_text = mention_detection_text(event);
        let pattern_mentioned = !routing_text.is_empty()
            && self
                .mention_patterns
                .iter()
                .any(|pattern| pattern.is_match(&routing_text).unwrap_or(false));
        // This gate precedes free-response and thread wake decisions in Python.
        // MPIM is shared conversation; only a one-to-one DM bypasses the gate.
        if self.ignore_other_mentions
            && channel_kind(event) != "im"
            && !bot.is_empty()
            && !pattern_mentioned
            && !mentions_user(event, &bot)
            && !mentions_self(&routing_text, &[&bot, &primary])
            && addressed_to_other(&routing_text, &[&bot, &primary])
        {
            return None;
        }
        if channel_kind(event) != "im"
            && !bot.is_empty()
            && self
                .strict_mentions
                .rejects(event, pattern_mentioned || mentions_user(event, &bot))
        {
            return None;
        }
        if (pattern_mentioned || (!bot.is_empty() && mentions_user(event, &bot)))
            && !self.strict_mentions.strict
            && !self.strict_mentions.thread
        {
            if let Some(thread) = self.session_thread(event) {
                self.thread_markers.lock().unwrap().remember_mention(
                    team.as_deref().unwrap_or(""),
                    &thread,
                    5000,
                );
            }
        }
        let payload = self.resolve_message_files(&payload).await?;
        let mut message = parse_permitted_message(&payload, &bot, pattern_mentioned)?;
        message.message_id = payload["event"]["ts"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        message.thread_id = self.session_thread(&payload["event"]);
        for file in payload["event"]["files"].as_array().into_iter().flatten() {
            let audio = audio_extension(file);
            let video = audio.is_none();
            let Some(extension) = audio.or_else(|| video_extension(file).map(str::to_owned)) else {
                continue;
            };
            let raw = file["url_private_download"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| file["url_private"].as_str())
                .unwrap_or("");
            match self
                .download_attachment(
                    event_team(&payload).as_deref().unwrap_or(""),
                    raw,
                    &extension,
                    video,
                )
                .await
            {
                Ok(path) => {
                    let paths = if video {
                        &mut message.video_paths
                    } else {
                        &mut message.audio_paths
                    };
                    paths.push(path.to_string_lossy().into_owned());
                }
                Err(_) => {
                    warn!(video, "slack media attachment download failed");
                    if !message.text.is_empty() {
                        message.text.push('\n');
                    }
                    message.text.push_str(if video {
                        "[Video attachment could not be downloaded.]"
                    } else {
                        "[Audio attachment could not be downloaded.]"
                    });
                }
            }
        }
        Some(message)
    }

    /// Mint a Socket Mode WebSocket URL via apps.connections.open.
    async fn open_connection(&self) -> Result<String> {
        let resp = self
            .client
            .post(format!("{}/apps.connections.open", self.api_base))
            .header("Authorization", format!("Bearer {}", self.app_token))
            .send()
            .await
            .map_err(|e| Error::Other(format!("slack apps.connections.open: {e}")))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("slack open decode: {e}")))?;
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Other(format!(
                "slack apps.connections.open not ok: {}",
                body.get("error").and_then(Value::as_str).unwrap_or("?")
            )));
        }
        body.get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Other("slack: open response missing url".into()))
    }

    async fn connect_and_run(&self, inbound: &mpsc::Sender<Message>) -> Result<()> {
        let url = self.open_connection().await?;
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| Error::Other(format!("slack connect: {e}")))?;
        let (mut write, mut read) = ws.split();

        // Poll pending handlers alongside socket input. In particular, the
        // lifecycle grace period must not delay ordinary share events or acks.
        let mut pending = futures_util::stream::FuturesUnordered::new();
        let result = async {
            loop {
                let frame = tokio::select! {
                    result = pending.next(), if !pending.is_empty() => {
                        if let Some(Some(msg)) = result {
                            if inbound.send(msg).await.is_err() {
                                debug!("slack: inbound channel closed, stopping");
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    frame = read.next() => match frame {
                        Some(frame) => frame,
                        None => break,
                    }
                };
                let frame = frame.map_err(|e| Error::Other(format!("slack read: {e}")))?;
                match frame {
                    WsMessage::Ping(p) => {
                        // Slack drops connections that don't pong.
                        let _ = write.send(WsMessage::Pong(p)).await;
                        continue;
                    }
                    WsMessage::Close(_) => {
                        return Err(Error::Other("slack: socket closed".into()));
                    }
                    _ => {}
                }
                let Some(payload) = parse_text_frame(&frame) else {
                    continue;
                };
                let Some(env) = parse_envelope(&payload) else {
                    continue;
                };

                // Ack anything that carries an envelope id, before handling it.
                if let Some(id) = &env.envelope_id {
                    let _ = write
                        .send(WsMessage::text(ack_payload(id).to_string()))
                        .await;
                }

                match env.kind.as_str() {
                    "events_api" => {
                        pending.push(self.prepare_event(env.payload));
                    }
                    // Slack asks us to reconnect (URL refresh, server rotation).
                    "disconnect" => {
                        return Err(Error::Other("slack: server requested disconnect".into()))
                    }
                    _ => {} // "hello" and others: nothing to do.
                }
            }
            Err(Error::Other("slack: socket stream ended".into()))
        }
        .await;
        // Envelopes have already been acknowledged. Finish their bounded HTTP
        // work before reconnecting rather than losing claimed share events.
        while let Some(message) = pending.next().await {
            if let Some(message) = message {
                if inbound.send(message).await.is_err() {
                    break;
                }
            }
        }
        result
    }
}

fn parse_text_frame(frame: &WsMessage) -> Option<Value> {
    match frame {
        WsMessage::Text(t) => serde_json::from_str(t).ok(),
        WsMessage::Binary(b) => serde_json::from_slice(b).ok(),
        _ => None,
    }
}

#[async_trait]
impl PlatformAdapter for SlackAdapter {
    fn name(&self) -> &str {
        "slack"
    }

    async fn run(&self, inbound: mpsc::Sender<Message>) -> Result<()> {
        loop {
            if self.register_workspaces().await.is_err() {
                warn!("slack workspace authentication failed; retrying after backoff");
                tokio::time::sleep(Duration::from_secs(5)).await;
                if inbound.is_closed() {
                    return Ok(());
                }
                continue;
            }
            if let Err(err) = self.connect_and_run(&inbound).await {
                warn!(%err, "slack socket cycle ended; reconnecting after backoff");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if inbound.is_closed() {
                return Ok(());
            }
        }
    }

    async fn send(&self, msg: &Message) -> Result<()> {
        if self.is_ignored_channel(&msg.channel_id) {
            return Err(Error::Other("ignored_channel".into()));
        }
        let team = msg
            .workspace_id
            .clone()
            .or_else(|| self.channel_teams.lock().unwrap().scope(&msg.channel_id));
        let mut body = json!({"channel": msg.channel_id, "text": msg.text});
        if let Some(thread) = self.reply_thread(msg) {
            body["thread_ts"] = json!(thread);
        }
        let resp = self
            .client
            .post(format!("{}/chat.postMessage", self.api_base))
            .bearer_auth(self.workspace_token(team.as_deref().unwrap_or(""), ""))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("slack chat.postMessage: {e}")))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("slack postMessage decode: {e}")))?;
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(Error::Other(format!(
                "slack chat.postMessage not ok: {}",
                body.get("error").and_then(Value::as_str).unwrap_or("?")
            )));
        }
        if let Some(timestamp) = body["ts"].as_str().filter(|s| !s.is_empty()) {
            self.thread_markers.lock().unwrap().remember_sent(
                team.as_deref().unwrap_or(""),
                timestamp,
                self.reply_thread(msg),
                5000,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_dedup_is_atomic_and_missing_timestamps_remain_deliverable() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-dedup-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            assert_eq!(event_dedup_key(&row["payload"]), row["result"], "{row}");
        }
        let adapter = SlackAdapter::new("app", "bot").unwrap();
        let payload = json!({"team_id":"T1","event":{"type":"message","channel":"C1","user":"U1","text":"hello","ts":"123.4"}});
        let (first, second) = tokio::join!(
            adapter.prepare_message(&payload),
            adapter.prepare_message(&payload)
        );
        assert_eq!(
            usize::from(first.is_some()) + usize::from(second.is_some()),
            1
        );
        let mut other = payload.clone();
        other["team_id"] = json!("T2");
        assert!(adapter.prepare_message(&other).await.is_some());
        other["event"]["ts"] = json!("");
        assert!(adapter.prepare_message(&other).await.is_some());
        assert!(adapter.prepare_message(&other).await.is_some());
        assert_eq!(
            event_dedup_key(
                &json!({"team_id":"outer","event":{"team_id":{"id":"inner"},"team":"ignored","ts":"x"}})
            ),
            "inner:x"
        );
        assert_eq!(
            event_dedup_key(&json!({"authorizations":[{"team_id":"auth"}],"event":{"ts":"x"}})),
            "auth:x"
        );
        assert_eq!(
            event_dedup_key(&json!({"event":{"ts":"old","_slack_changed_event_ts":"new"}})),
            "new"
        );
        assert_eq!(dedup_ttl(None), 3600.0);
        assert_eq!(dedup_ttl(Some("0")), 3600.0);
        assert_eq!(dedup_ttl(Some("bad")), 3600.0);
        assert_eq!(dedup_ttl(Some("1_200")), 1200.0);
        assert_eq!(dedup_ttl(Some("NaN")), 3600.0);
    }

    #[tokio::test]
    async fn slack_connect_stubs_resolve_before_classification_without_mutating_events() {
        use axum::{extract::Query, http::HeaderMap, routing::get, Json, Router};
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app = Router::new().route("/files.info", get(move |headers: HeaderMap, Query(params): Query<std::collections::HashMap<String,String>>| {
            let id = params["file"].clone();
            recorded.lock().unwrap().push(id.clone());
            async move {
                assert_eq!(headers["authorization"], "Bearer bot-fixture");
                if id == "denied" { return Json(json!({"ok":false,"error":"private-error-marker"})); }
                Json(json!({"ok":true,"file":{"id":id,"name":"clip.mp4","mimetype":"video/mp4","subtype":"slack_audio","url_private":"https://invalid.example/blocked"}}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut adapter = SlackAdapter::new("app-fixture", "bot-fixture").unwrap();
        adapter.api_base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let payload = json!({"event":{"type":"message","subtype":"file_share","user":"U1","channel":"C1","text":"","files":[
            {"id":"first","file_access":"check_file_info"},
            {"id":"complete","mimetype":"audio/ogg"},
            {"file_access":"check_file_info"},
            {"id":"last","file_access":"check_file_info"}
        ]}});
        let before = payload.clone();
        let resolved = adapter.resolve_message_files(&payload).await.unwrap();
        assert_eq!(payload, before);
        assert_eq!(*calls.lock().unwrap(), vec!["first", "last"]);
        assert_eq!(
            resolved["event"]["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["first", "complete", "last"]
        );
        assert_eq!(
            audio_extension(&resolved["event"]["files"][0]).as_deref(),
            Some(".mp4")
        );
        assert!(parse_message_event(&resolved).is_some());
        let mut rejected = payload.clone();
        rejected["event"]["bot_id"] = json!("B1");
        assert!(adapter.prepare_message(&rejected).await.is_none());
        assert_eq!(calls.lock().unwrap().len(), 2);
        let mut failed = payload.clone();
        failed["event"]["text"] = json!("keep caption");
        failed["event"]["files"] = json!([{"id":"denied","file_access":"check_file_info"}]);
        let message = adapter.prepare_message(&failed).await.unwrap();
        assert!(message.text.starts_with("keep caption\n"));
        assert!(!message.text.contains("private-error-marker"));
        assert!(message.audio_paths.is_empty());
        let mut success = payload.clone();
        success["event"]["ts"] = json!("same-share");
        success["event"]["files"] = json!([{"id":"first","file_access":"check_file_info"}]);
        let message = adapter.prepare_message(&success).await.unwrap();
        // The resolved voice file reaches download admission, which rejects the
        // fixture's non-Slack URL without any second network request.
        assert!(message.text.contains("could not be downloaded"));
        let call_count = calls.lock().unwrap().len();
        assert!(adapter.prepare_message(&success).await.is_none());
        assert_eq!(calls.lock().unwrap().len(), call_count);
        assert!(adapter.prepare_message(&json!({"event":{"type":"file_shared","file_id":"first","channel":"C1","user":"U1"}})).await.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn lifecycle_fallback_keeps_socket_responsive_and_shares_dedup() {
        use axum::{
            extract::Query,
            routing::{get, post},
            Json, Router,
        };
        use std::collections::HashMap;
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", socket.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/apps.connections.open",
                post(move || {
                    let url = url.clone();
                    async move { Json(json!({"ok":true,"url":url})) }
                }),
            )
            .route(
                "/files.info",
                get(|Query(query): Query<HashMap<String, String>>| async move {
                    let id = &query["file"];
                    Json(
                        json!({"ok":true,"file":{"id":id,"user":"U1","mimetype":"video/mp4",
                            "url_private":"https://invalid.example/private",
                            "shares":{"public":{"C1":[{"ts":id}]}}
                        }}),
                    )
                }),
            );
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", http.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(http, app).await.unwrap();
        });
        let mut adapter = SlackAdapter::new("app-fixture", "bot-fixture").unwrap();
        adapter.api_base = api;
        let (tx, mut rx) = mpsc::channel(8);
        let runner = tokio::spawn(async move { adapter.connect_and_run(&tx).await });
        let (stream, _) = socket.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let lifecycle = |id: &str, envelope: &str| {
            json!({"type":"events_api","envelope_id":envelope,
            "payload":{"team_id":"T1","event":{"type":"file_shared","channel_id":"C1","file_id":id,"event_ts":"different-lifecycle-ts"}}})
        };
        ws.send(WsMessage::text(lifecycle("share", "fallback").to_string()))
            .await
            .unwrap();
        let ack = ws.next().await.unwrap().unwrap();
        assert_eq!(parse_text_frame(&ack).unwrap()["envelope_id"], "fallback");
        // Allow files.info to start, then deliver the normal event while the
        // fallback is sleeping. A serial handler would miss this deadline.
        tokio::time::sleep(Duration::from_millis(100)).await;
        ws.send(WsMessage::text(json!({"type":"events_api","envelope_id":"normal",
            "payload":{"team_id":"T1","event":{"type":"message","subtype":"file_share","channel":"C1","user":"U1","ts":"share","text":"normal caption"}}}).to_string())).await.unwrap();
        let ack = tokio::time::timeout(Duration::from_millis(400), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(parse_text_frame(&ack).unwrap()["envelope_id"], "normal");
        let msg = tokio::time::timeout(Duration::from_millis(400), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.text, "normal caption");
        assert!(tokio::time::timeout(Duration::from_millis(850), rx.recv())
            .await
            .is_err());

        // A lifecycle-only video must produce a turn. The private URL is
        // deliberately rejected, so the turn carries a generic failure note.
        ws.send(WsMessage::text(lifecycle("only", "only").to_string()))
            .await
            .unwrap();
        ws.next().await.unwrap().unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.text, "[Video attachment could not be downloaded.]");
        assert_eq!(msg.sender_id, "U1");
        assert!(msg.video_paths.is_empty());
        ws.send(WsMessage::text(lifecycle("only", "repeat").to_string()))
            .await
            .unwrap();
        ws.next().await.unwrap().unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(900), rx.recv())
            .await
            .is_err());
        ws.send(WsMessage::text(lifecycle("closing", "closing").to_string()))
            .await
            .unwrap();
        ws.next().await.unwrap().unwrap();
        ws.close(None).await.unwrap();
        let closing = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(closing.text, "[Video attachment could not be downloaded.]");
        assert!(tokio::time::timeout(Duration::from_secs(3), runner)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        server.abort();
    }

    #[tokio::test]
    async fn workspace_credentials_route_http_and_isolate_history() {
        use axum::{
            http::{HeaderMap, Uri},
            response::IntoResponse,
            routing::{get, post},
            Router,
        };
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let handler = move |uri: Uri, headers: HeaderMap| {
            let calls = recorded.clone();
            async move {
                let auth = headers["authorization"].to_str().unwrap().to_owned();
                calls
                    .lock()
                    .unwrap()
                    .push((uri.path().to_owned(), auth.clone()));
                match uri.path() {
                    "/auth.test" => {
                        let team = match auth.as_str() {
                            "Bearer one" | "Bearer one-rotated" => "T1",
                            "Bearer two" => "T2",
                            "Bearer three" => "T3",
                            _ => "",
                        };
                        axum::Json(json!({"ok":!team.is_empty(),"team_id":team,"user_id":if auth == "Bearer one-rotated" { "BOT_ALT".into() } else { format!("BOT_{team}") }})).into_response()
                    }
                    "/files.info" => {
                        axum::Json(json!({"ok":true,"file":{"id":"F1"}})).into_response()
                    }
                    "/chat.postMessage" => axum::Json(json!({"ok":true})).into_response(),
                    _ => "OggSfixture".into_response(),
                }
            }
        };
        let app = Router::new()
            .route("/auth.test", post(handler.clone()))
            .route("/files.info", get(handler.clone()))
            .route("/chat.postMessage", post(handler.clone()))
            .route("/audio", get(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let home = std::env::temp_dir().join(format!(
            "slack-workspaces-{}",
            crate::install_identity::mint_id().unwrap()
        ));
        tokio::fs::create_dir_all(&home).await.unwrap();
        tokio::fs::write(
            home.join("slack_tokens.json"),
            br#"{"untrusted-team-label":{"token":"three"},"duplicate":{"token":"two"}}"#,
        )
        .await
        .unwrap();
        let mut adapter = SlackAdapter::new("app", " one, two, ")
            .unwrap()
            .with_audio_cache(home.clone(), &json!({}));
        adapter.api_base = base.clone();
        adapter.register_workspaces().await.unwrap();
        assert_eq!(calls.lock().unwrap().len(), 3);
        assert_eq!(
            adapter.workspace_token("T2", "/files-pri/T3-F1/clip"),
            "two"
        );
        assert_eq!(
            adapter.workspace_token("missing", "/files-pri/T3-F1/clip"),
            "three"
        );
        assert_eq!(adapter.workspace_token("", "/files-pri/t3-F1/clip"), "one");
        assert!(adapter.is_self_sender(Some("T2"), "BOT_T2"));
        assert!(!adapter.is_self_sender(Some("T2"), "BOT_T1"));
        assert!(adapter.is_self_sender(None, "BOT_T1"));
        let self_post = json!({"team_id":"T2","event":{"type":"message","channel":"self-channel","user":"BOT_T2","text":"my status","files":[{"id":"F1","file_access":"check_file_info"}]}});
        assert!(adapter.prepare_message(&self_post).await.is_none());
        let mut inferred_self = self_post.clone();
        inferred_self.as_object_mut().unwrap().remove("team_id");
        assert!(adapter.prepare_message(&inferred_self).await.is_none());
        assert_eq!(
            calls.lock().unwrap().len(),
            3,
            "self posts must not fetch file metadata"
        );
        adapter.file_info("T3", "F1").await.unwrap();
        adapter
            .fetch_attachment(
                "T2",
                &adapter.client,
                format!("{base}/audio").parse().unwrap(),
                ".ogg",
                false,
            )
            .await
            .unwrap();
        let mut message = parse_message_event(&json!({"team_id":"T2","event":{"type":"message","channel":"same","user":"U1","text":"second workspace"}})).unwrap();
        adapter.send(&message).await.unwrap();
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls[3], ("/files.info".into(), "Bearer three".into()));
            assert_eq!(calls[4], ("/audio".into(), "Bearer two".into()));
            assert_eq!(calls[5], ("/chat.postMessage".into(), "Bearer two".into()));
        }
        adapter.ignored_channels.insert("same".into());
        let ignored = json!({"event":{"type":"message","channel":"same:123.4","user":"U1","files":[{"id":"F1","file_access":"check_file_info"}]}});
        assert!(adapter.prepare_message(&ignored).await.is_none());
        assert!(adapter
            .prepare_event(
                json!({"event":{"type":"file_shared","channel_id":"same","file_id":"F1"}})
            )
            .await
            .is_none());
        assert!(adapter.send(&message).await.is_err());
        assert_eq!(
            calls.lock().unwrap().len(),
            6,
            "ignored events must not issue HTTP requests"
        );
        adapter.ignored_channels.clear();
        adapter.allowed_channels = ["allowed".into()].into_iter().collect();
        let blocked = json!({"team_id":"T2","event":{"type":"message","channel":"blocked","user":"PEER","files":[{"id":"F1","file_access":"check_file_info"}]}});
        assert!(adapter.prepare_message(&blocked).await.is_none());
        adapter.disable_dms = true;
        let mut dm = blocked;
        dm["event"]["channel"] = json!("D1");
        assert!(adapter.prepare_message(&dm).await.is_none());
        assert_eq!(
            calls.lock().unwrap().len(),
            6,
            "channel controls precede user/file lookups"
        );
        adapter.allowed_channels.clear();
        adapter.disable_dms = false;
        let db = crate::session_db::SessionDb::open(home.join("sessions.db")).unwrap();
        assert!(crate::session_db::begin_turn(Some(&db), false, &message, "slack").is_empty());
        crate::session_db::end_turn(Some(&db), false, &message, "second reply");
        message.workspace_id = Some("T1".into());
        assert!(crate::session_db::begin_turn(Some(&db), false, &message, "slack").is_empty());
        message.workspace_id = Some("T2".into());
        assert_eq!(
            crate::session_db::begin_turn(Some(&db), false, &message, "slack").len(),
            2
        );
        // Failed reauthentication leaves the previously complete map intact.
        adapter.bot_tokens.push("invalid".into());
        assert!(adapter.register_workspaces().await.is_err());
        assert_eq!(adapter.workspace_token("T3", ""), "three");
        assert!(adapter.is_self_sender(Some("T3"), "BOT_T3"));
        adapter.bot_tokens.pop();
        adapter.bot_tokens.push("one-rotated".into());
        adapter.register_workspaces().await.unwrap();
        assert_eq!(adapter.workspace_token("T1", ""), "one-rotated");
        assert!(adapter.is_self_sender(Some("T1"), "BOT_ALT"));
        assert!(
            adapter.is_self_sender(None, "BOT_T1"),
            "first token keeps the primary identity"
        );
        drop(db);
        server.abort();
        tokio::fs::remove_dir_all(home).await.unwrap();
    }

    #[tokio::test]
    async fn channel_inference_matches_python_and_does_not_guess_ambiguous_routes() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-channel-goldens.json"))
                .unwrap();
        let mut cache = ChannelTeams::default();
        for row in rows.as_array().unwrap() {
            cache.remember(
                row["channel"].as_str().unwrap(),
                row["team"].as_str().unwrap(),
                4,
            );
            assert_eq!(json!(cache.routes), row["routes"], "{row}");
            assert_eq!(json!(cache.observed), row["observed"], "{row}");
        }
        let adapter = SlackAdapter::new("app", "bot").unwrap();
        let event = |team: Option<&str>, ts: &str| {
            let mut payload = json!({"event":{"type":"message","channel":"C1","user":"U1","ts":ts,"text":"caption"}});
            if let Some(team) = team {
                payload["team_id"] = json!(team);
            }
            payload
        };
        let first = adapter
            .prepare_message(&event(Some("T1"), "one"))
            .await
            .unwrap();
        assert_eq!(first.workspace_id.as_deref(), Some("T1"));
        // The delivered identity is claimed before channel inference, as in Python.
        assert!(adapter.prepare_message(&event(None, "one")).await.is_some());
        assert!(adapter.prepare_message(&event(None, "one")).await.is_none());
        let inferred = adapter.prepare_message(&event(None, "two")).await.unwrap();
        assert_eq!(inferred.workspace_id.as_deref(), Some("T1"));
        let other = adapter
            .prepare_message(&event(Some("T2"), "one"))
            .await
            .unwrap();
        assert_eq!(other.workspace_id.as_deref(), Some("T2"));
        let ambiguous = adapter
            .prepare_message(&event(None, "three"))
            .await
            .unwrap();
        assert!(ambiguous.workspace_id.is_none());
        let explicit = adapter
            .prepare_message(&event(Some("T1"), "four"))
            .await
            .unwrap();
        assert_eq!(explicit.workspace_id.as_deref(), Some("T1"));
        assert!(adapter.channel_teams.lock().unwrap().scope("C1").is_none());
    }

    #[test]
    fn ignored_channel_policy_matches_python_and_configuration_precedence() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-ignored-goldens.json"))
                .unwrap();
        let mut adapter = SlackAdapter::new("app", "bot").unwrap();
        for row in rows.as_array().unwrap() {
            adapter.ignored_channels = ignored_channels(
                &json!({"platforms":{"slack":{"extra":{"ignored_channels":row["extra"]}}}}),
                row["legacy"].as_str(),
            );
            assert_eq!(
                adapter.is_ignored_channel(row["channel"].as_str().unwrap()),
                row["result"].as_bool().unwrap(),
                "{row}"
            );
        }
        let config = json!({"slack":{"ignored_channels":["C1,C2", " C3 "]}});
        assert_eq!(
            ignored_channels(&config, None),
            ["C1", "C2", "C3"].map(str::to_owned).into_iter().collect()
        );
        assert_eq!(
            ignored_channels(&config, Some("C4")),
            ["C4".into()].into_iter().collect()
        );
        assert_eq!(
            ignored_channels(&config, Some("")),
            ignored_channels(&config, None)
        );
        let config = json!({"slack":{"ignored_channels":"*"},"platforms":{"slack":{"extra":{"ignored_channels":[]}}}});
        assert!(ignored_channels(&config, Some("*")).is_empty());
    }

    #[tokio::test]
    async fn declared_bot_modes_preserve_authored_mentions_and_self_rejection() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-bot-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            let allowed = row["allowed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(
                declares_bot(&row["event"], &allowed),
                row["bot"].as_bool().unwrap(),
                "{row}"
            );
            assert_eq!(
                mentions_user(&row["event"], "BOT"),
                row["mentioned"].as_bool().unwrap(),
                "{row}"
            );
        }
        let mut adapter = SlackAdapter::new("app", "bot").unwrap();
        adapter.allow_bots = "none".into();
        adapter.team_tokens.lock().unwrap().primary_bot_user_id = "BOT".into();
        let mut payload = json!({"event":{"type":"message","subtype":"bot_message","bot_id":"B1","user":"PEER","channel":"C1","text":"caption"}});
        assert!(adapter.prepare_message(&payload).await.is_none());
        adapter.allow_bots = "all".into();
        assert_eq!(
            adapter.prepare_message(&payload).await.unwrap().text,
            "caption"
        );
        adapter.allow_bots = "mentions".into();
        payload["event"]["blocks"] =
            json!([{"type":"rich_text_quote","elements":[{"type":"user","user_id":"BOT"}]}]);
        assert!(adapter.prepare_message(&payload).await.is_none());
        payload["event"]["blocks"][0]["type"] = json!("rich_text_section");
        assert!(adapter.prepare_message(&payload).await.is_some());
        payload["event"]["user"] = json!("BOT");
        adapter.allow_bots = "all".into();
        assert!(adapter.prepare_message(&payload).await.is_none());
        assert_eq!(
            bot_policy(&json!({"slack":{"allow_bots":"all"}}), Some("mentions")),
            "mentions"
        );
        assert_eq!(
            bot_policy(
                &json!({"platforms":{"slack":{"extra":{"allow_bots":" ALL "}}}}),
                Some("none")
            ),
            "all"
        );
        assert_eq!(bot_policy(&json!({}), Some("invalid")), "none");
    }

    #[tokio::test]
    async fn user_bot_lookup_is_cached_per_workspace_and_controls_turn_admission() {
        use axum::{extract::Query, http::HeaderMap, routing::get, Json, Router};
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app = Router::new().route("/users.info", get(move |headers: HeaderMap, Query(query): Query<std::collections::HashMap<String, String>>| {
            let calls = recorded.clone();
            async move {
                let token = headers["authorization"].to_str().unwrap().to_owned();
                let user = query["user"].clone();
                calls.lock().unwrap().push((token.clone(), user.clone()));
                Json(if user == "failed" { json!({"ok":false}) }
                    else if user == "workflow" { json!({"ok":true,"user":{"is_workflow_bot":true}}) }
                    else if user == "profile" { json!({"ok":true,"user":{"profile":{"bot_id":"B"}}}) }
                    else { json!({"ok":true,"user":{"is_bot":user == "peer" && token == "Bearer two"}}) })
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut adapter = SlackAdapter::new("app", "one").unwrap();
        adapter.api_base = base;
        adapter.allow_bots = "none".into();
        *adapter.team_tokens.lock().unwrap() = WorkspaceCredentials {
            primary_bot_user_id: "BOT1".into(),
            teams: [
                (
                    "T1".into(),
                    WorkspaceCredential {
                        token: "one".into(),
                        bot_user_id: "BOT1".into(),
                    },
                ),
                (
                    "T2".into(),
                    WorkspaceCredential {
                        token: "two".into(),
                        bot_user_id: "BOT2".into(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };
        let mut payload = json!({"team_id":"T2","event":{"type":"message","channel":"C1","user":"peer","text":"status","client_msg_id":"client"}});
        assert!(adapter.prepare_message(&payload).await.is_none());
        assert!(adapter.prepare_message(&payload).await.is_none());
        assert_eq!(calls.lock().unwrap().len(), 1);
        adapter.allow_bots = "mentions".into();
        payload["event"]["text"] = json!("<@BOT2> help");
        assert!(adapter.prepare_message(&payload).await.is_some());
        payload["event"]
            .as_object_mut()
            .unwrap()
            .remove("client_msg_id");
        assert!(
            adapter.prepare_message(&payload).await.is_none(),
            "suspicious events also pass the primary mention gate"
        );
        payload["event"]["text"] = json!("<@BOT1> <@BOT2> help");
        assert!(adapter.prepare_message(&payload).await.is_some());
        payload["team_id"] = json!("T1");
        adapter.allow_bots = "none".into();
        assert!(adapter.prepare_message(&payload).await.is_some());
        assert_eq!(
            calls.lock().unwrap()[1],
            ("Bearer one".into(), "peer".into())
        );
        assert!(adapter.resolve_user_is_bot("T2", "workflow").await);
        assert!(adapter.resolve_user_is_bot("T2", "profile").await);
        assert!(!adapter.resolve_user_is_bot("T2", "failed").await);
        let count = calls.lock().unwrap().len();
        assert!(!adapter.resolve_user_is_bot("T2", "failed").await);
        assert_eq!(calls.lock().unwrap().len(), count);
        server.abort();
    }

    #[tokio::test]
    async fn block_only_ui_messages_reach_the_model_without_changing_commands() {
        let adapter = SlackAdapter::new("app", "bot").unwrap();
        let mut payload = json!({"event":{"type":"message","channel":"C1","user":"U1","text":"","blocks":[
            {"type":"section","text":{"type":"mrkdwn","text":"Inspect the deployment"}},
            {"type":"actions","elements":[{"type":"button","action_id":"inspect","text":{"type":"plain_text","text":"Inspect"},"url":"https://private.invalid/secret","value":"hidden"}]}
        ]}});
        let message = adapter.prepare_message(&payload).await.unwrap();
        assert!(message
            .model_content()
            .as_str()
            .unwrap()
            .contains("Inspect the deployment"));
        assert!(message.text.contains("inspect"));
        assert!(!message.text.contains("private.invalid"));
        assert!(!message.text.contains("hidden"));
        let preview = json!({"event":{"type":"message","channel":"C1","user":"U1","text":"https://example.com","attachments":[{"title":"Report","title_link":"https://example.com","text":"Useful preview","footer":"Example"}]}});
        assert_eq!(adapter.prepare_message(&preview).await.unwrap().text, "https://example.com\n\n📎 [Report](https://example.com)\n   Useful preview\n   _Example_");
        let mut command_preview = preview.clone();
        command_preview["event"]["text"] = json!("/help");
        assert_eq!(
            adapter
                .prepare_message(&command_preview)
                .await
                .unwrap()
                .text,
            "/help"
        );
        let mut only_preview = preview;
        only_preview["event"]["text"] = json!("");
        assert!(adapter
            .prepare_message(&only_preview)
            .await
            .unwrap()
            .text
            .starts_with("📎 [Report]"));
        let quote = json!({"event":{"type":"message","channel":"C1","user":"U1","text":"hello","blocks":[{"type":"rich_text","elements":[
            {"type":"rich_text_section","elements":[{"type":"text","text":"hello"}]},
            {"type":"rich_text_quote","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"forwarded detail"}]}]}
        ]}]}});
        assert_eq!(
            adapter.prepare_message(&quote).await.unwrap().text,
            "hello\n> forwarded detail"
        );
        let mut block_only = quote;
        block_only["event"]["text"] = json!("");
        assert_eq!(
            adapter.prepare_message(&block_only).await.unwrap().text,
            "hello\n> forwarded detail"
        );
        payload["event"]["text"] = json!("/help");
        assert_eq!(
            adapter.prepare_message(&payload).await.unwrap().text,
            "/help"
        );
        payload["event"]["text"] = json!("caption");
        payload["event"]["blocks"] =
            json!([{"type":"rich_text","elements":[{"type":"text","text":"caption"}]}]);
        assert_eq!(
            adapter.prepare_message(&payload).await.unwrap().text,
            "caption"
        );
    }

    #[tokio::test]
    async fn bang_commands_match_python_and_skip_mirrored_blocks() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-bang-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            assert_eq!(
                rewrite_bang(row["text"].as_str().unwrap()),
                row["result"].as_str().unwrap(),
                "{row}"
            );
        }
        let adapter = SlackAdapter::new("app", "bot").unwrap();
        let payload = json!({"event":{"type":"message","channel":"C1","user":"U1","text":"  !help","blocks":[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"!help"}]}]}]}});
        let message = adapter.prepare_message(&payload).await.unwrap();
        assert_eq!(message.text, "/help");
        assert_eq!(
            crate::slash::command_name(&message.text).as_deref(),
            Some("help")
        );
        assert!(crate::slash::handle_builtin("help", &message, &json!({})).is_some());
        assert_eq!(rewrite_bang("!nice work"), "!nice work");

        adapter.team_tokens.lock().unwrap().primary_bot_user_id = "UBOT".into();
        for (input, expected) in [
            ("<@UBOT> !help", "/help"),
            ("  <@UBOT> /help topic  ", "/help topic"),
            ("<@UBOT> !HELP@hermes topic", "/HELP@hermes topic"),
        ] {
            let mut addressed = payload.clone();
            addressed["event"]["text"] = json!(input);
            addressed["event"]["attachments"] = json!([{"text":"Unrelated preview"}]);
            let message = adapter.prepare_message(&addressed).await.unwrap();
            assert_eq!(message.text, expected, "{input}");
            assert_eq!(
                crate::slash::command_name(&message.text).as_deref(),
                Some("help")
            );
        }
        let ordinary = json!({"event":{"type":"message","channel":"C1","user":"U1","text":"<@UBOT> !nice work"}});
        assert_eq!(
            adapter.prepare_message(&ordinary).await.unwrap().text,
            "!nice work"
        );
        let other =
            json!({"event":{"type":"message","channel":"C1","user":"U1","text":"<@OTHER> !help"}});
        assert_eq!(
            adapter.prepare_message(&other).await.unwrap().text,
            "<@OTHER> !help"
        );
    }

    #[tokio::test]
    async fn strict_mentions_match_python_and_reject_before_enrichment() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-strict-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            let flag = |key: &str| row[key].as_bool().unwrap();
            let channels = |key: &str| {
                if flag(key) {
                    ["C1".to_owned()].into_iter().collect()
                } else {
                    Default::default()
                }
            };
            let policy = StrictMentionPolicy {
                require: flag("require"),
                strict: flag("strict"),
                thread: flag("thread"),
                free_channels: channels("free"),
                required_channels: channels("required"),
            };
            let event = json!({"channel":"C1", "ts":"2", "thread_ts":if flag("reply") {"1"} else {""}, "_hermes_force_process":flag("force")});
            assert_eq!(
                policy.rejects(&event, flag("mentioned")),
                flag("rejected"),
                "{row}"
            );
        }
        let config = json!({"platforms":{"slack":{"extra":{
            "require_mention":true, "strict_mention":true, "thread_require_mention":true,
            "free_response_channels":["C1"], "require_mention_channels":[]
        }}}});
        let mut adapter = SlackAdapter::new("app", "bot").unwrap();
        adapter.strict_mentions = StrictMentionPolicy::from_config(&config);
        adapter.team_tokens.lock().unwrap().primary_bot_user_id = "BOT".into();
        let mut payload = json!({"event":{"type":"message","channel":"C1","channel_type":"channel","user":"HUMAN","text":"hello","thread_ts":"1","files":[{"file_access":"check_file_info"}]}});
        assert!(adapter.prepare_message(&payload).await.is_none());
        payload["event"]["files"] = json!([]);
        payload["event"]["thread_ts"] = json!("");
        assert!(adapter.prepare_message(&payload).await.is_some());
        adapter
            .strict_mentions
            .required_channels
            .insert("C1".into());
        assert!(adapter.prepare_message(&payload).await.is_none());
        payload["event"]["text"] = json!("<@BOT> hello");
        assert_eq!(
            adapter.prepare_message(&payload).await.unwrap().text,
            "hello"
        );
        payload["event"]["text"] = json!("hello");
        payload["event"]["_hermes_force_process"] = json!(true);
        assert!(adapter.prepare_message(&payload).await.is_some());
        payload["event"]["_hermes_force_process"] = json!(false);
        payload["event"]["channel_type"] = json!("im");
        assert!(adapter.prepare_message(&payload).await.is_some());
        payload["event"]["channel_type"] = json!("mpim");
        assert!(adapter.prepare_message(&payload).await.is_none());

        assert!(mention_flag(
            &Value::Null,
            "require_mention",
            Some(" false "),
            true
        ));
        assert!(!mention_flag(
            &Value::Null,
            "require_mention",
            Some("OFF"),
            true
        ));
        assert!(mention_flag(
            &Value::Null,
            "require_mention",
            Some(""),
            true
        ));
        assert!(!mention_flag(
            &json!({"slack":{"strict_mention":2}}),
            "strict_mention",
            None,
            false
        ));
        assert!(mention_flag(
            &json!({"platforms":{"slack":{"extra":{"strict_mention":2}}}}),
            "strict_mention",
            None,
            false
        ));
    }

    #[tokio::test]
    async fn addressed_user_gate_matches_python_and_keeps_dm_and_wake_exceptions() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-addressed-goldens.json"))
                .unwrap();
        for row in rows["messages"].as_array().unwrap() {
            let routing = mention_detection_text(&row["event"]);
            assert_eq!(routing, row["routing"].as_str().unwrap(), "{row}");
            assert_eq!(
                addressed_to_other(&routing, &["BOT", "PRIMARY"]),
                row["addressed"].as_bool().unwrap(),
                "{row}"
            );
            assert_eq!(
                mentions_self(&routing, &["BOT", "PRIMARY"]),
                row["mentions_self"].as_bool().unwrap(),
                "{row}"
            );
        }
        for row in rows["configs"].as_array().unwrap() {
            let config = json!({"platforms":{"slack":{"extra":{"ignore_other_user_mentions":row["extra"]}}}});
            assert_eq!(
                ignore_other_mentions(&config, row["legacy"].as_str()),
                row["result"].as_bool().unwrap(),
                "{row}"
            );
        }
        // Exercise rejection before file hydration: the deliberately incomplete
        // Connect file would otherwise become a visible metadata failure note.
        let mut adapter = SlackAdapter::new("app", "bot").unwrap();
        adapter.ignore_other_mentions = true;
        adapter.team_tokens.lock().unwrap().primary_bot_user_id = "BOT".into();
        let mut payload = json!({"event":{"type":"message","channel":"C1","channel_type":"channel","user":"HUMAN","text":"<@OTHER> hi","files":[{"file_access":"check_file_info"}]}});
        assert!(adapter.prepare_message(&payload).await.is_none());
        payload["event"]["files"] = json!([]);
        payload["event"]["channel_type"] = json!("mpim");
        assert!(adapter.prepare_message(&payload).await.is_none());
        payload["event"]["channel_type"] = json!("im");
        assert!(adapter.prepare_message(&payload).await.is_some());
        payload["event"]["channel_type"] = json!("channel");
        payload["event"]["text"] = json!("<@OTHER> <@BOT|Hermes> hi");
        assert!(adapter.prepare_message(&payload).await.is_some());
        payload["event"]["text"] = json!("<@OTHER> Hermes help");
        adapter.mention_patterns = mention_patterns(
            &json!({"platforms":{"slack":{"extra":{"mention_patterns":["\\bhermes\\b"]}}}}),
            None,
        );
        assert!(adapter.prepare_message(&payload).await.is_some());
        adapter.mention_patterns.clear();
        assert!(adapter.prepare_message(&payload).await.is_none());
        payload["event"]["text"] = json!("ask <@OTHER>");
        assert!(adapter.prepare_message(&payload).await.is_some());

        // The resolved-user gate accepts wake words, while the earlier declared
        // bot gate still requires a literal primary mention in mentions mode.
        adapter.allow_bots = "mentions".into();
        adapter.mention_patterns =
            mention_patterns(&Value::Null, Some("[\"(?<=hello )hermes\", \"[\", 12]"));
        let bot_event = json!({"text":"hello HERMES"});
        assert!(adapter.permits_bot(&bot_event, None, false));
        assert!(!adapter.permits_bot(&bot_event, None, true));
        assert_eq!(
            mention_patterns(&Value::Null, Some("one, two\nthree")).len(),
            3
        );
        assert!(mention_patterns(&Value::Null, Some("false")).is_empty());
        let config = json!({"platforms":{"slack":{"extra":{"mention_patterns":[]}}}});
        assert!(mention_patterns(&config, Some("hermes")).is_empty());
    }

    #[tokio::test]
    async fn media_redirects_validate_each_hop_and_do_not_restore_credentials() {
        use axum::{
            http::{HeaderMap, StatusCode, Uri},
            response::IntoResponse,
            routing::get,
            Router,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app = Router::new().fallback(get(move |uri: Uri, headers: HeaderMap| {
            let calls = recorded.clone();
            async move {
                calls.lock().unwrap().push((
                    uri.path().to_owned(),
                    headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                ));
                let target = match uri.path() {
                    "/same" => Some("/final".to_owned()),
                    "/cross" => Some(format!("http://cdn.example:{}/back", address.port())),
                    "/back" => Some(format!("http://files.slack.com:{}/final", address.port())),
                    "/private" => Some("http://169.254.169.254/latest".into()),
                    "/userinfo" => Some("http://user:secret@cdn.example/final".into()),
                    "/loop" => Some("/loop".into()),
                    _ => None,
                };
                match target {
                    Some(target) => (StatusCode::FOUND, [("location", target)]).into_response(),
                    None => "OggSfixture".into_response(),
                }
            }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("files.slack.com", address)
            .resolve("cdn.example", address)
            .build()
            .unwrap();
        // Test transport maps named origins to localhost. Production uses the
        // same loop with pinned_media_client, never this DNS override.
        let clients = |url: reqwest::Url| {
            let client = client.clone();
            async move {
                if url.host_str() == Some("169.254.169.254") {
                    pinned_media_client(url).await
                } else {
                    Ok(client)
                }
            }
        };
        let url = |path: &str| {
            format!("http://files.slack.com:{}{path}", address.port())
                .parse()
                .unwrap()
        };
        let home = std::env::temp_dir().join(format!(
            "slack-redirect-{}",
            crate::install_identity::mint_id().unwrap()
        ));
        let adapter = SlackAdapter::new("app", "fixture")
            .unwrap()
            .with_audio_cache(home.clone(), &json!({}));
        let response = follow_media_redirects(
            url("/same"),
            "fixture".into(),
            &reqwest::cookie::Jar::default(),
            clients,
        )
        .await
        .unwrap();
        let path = adapter
            .cache_attachment(response, ".ogg", false)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"OggSfixture");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                ("/same".into(), Some("Bearer fixture".into())),
                ("/final".into(), Some("Bearer fixture".into()))
            ]
        );
        calls.lock().unwrap().clear();
        follow_media_redirects(
            url("/cross"),
            "fixture".into(),
            &reqwest::cookie::Jar::default(),
            clients,
        )
        .await
        .unwrap();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                ("/cross".into(), Some("Bearer fixture".into())),
                ("/back".into(), None),
                ("/final".into(), None)
            ]
        );
        for path in ["/private", "/userinfo"] {
            calls.lock().unwrap().clear();
            assert!(follow_media_redirects(
                url(path),
                "fixture".into(),
                &reqwest::cookie::Jar::default(),
                clients
            )
            .await
            .is_err());
            assert_eq!(
                calls.lock().unwrap().len(),
                1,
                "rejected destination must not receive a request"
            );
        }
        calls.lock().unwrap().clear();
        assert!(follow_media_redirects(
            url("/loop"),
            "fixture".into(),
            &reqwest::cookie::Jar::default(),
            clients
        )
        .await
        .is_err());
        assert_eq!(calls.lock().unwrap().len(), 21);
        server.abort();
        tokio::fs::remove_dir_all(home).await.unwrap();
    }

    #[tokio::test]
    async fn media_retries_status_failures_but_not_html_or_access_denials() {
        use axum::{
            http::{StatusCode, Uri},
            response::IntoResponse,
            routing::get,
            Router,
        };
        let counts = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            usize,
        >::new()));
        let observed = counts.clone();
        let app = Router::new().fallback(get(move |uri: Uri| {
            let counts = observed.clone();
            async move {
                let count = {
                    let mut counts = counts.lock().unwrap();
                    let n = counts.entry(uri.path().into()).or_default();
                    *n += 1;
                    *n
                };
                if uri.path() == "/slow" {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                match uri.path() {
                    "/retry" if count < 3 => (
                        if count == 1 {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::TOO_MANY_REQUESTS
                        },
                        [("content-type", "text/html")],
                        "retry later",
                    )
                        .into_response(),
                    "/denied" => (StatusCode::FORBIDDEN, "denied").into_response(),
                    "/html" => ([("content-type", "text/html")], "login").into_response(),
                    _ => "OggSfixture".into_response(),
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let home = std::env::temp_dir().join(format!(
            "slack-retry-{}",
            crate::install_identity::mint_id().unwrap()
        ));
        let adapter = SlackAdapter::new("app", "fixture")
            .unwrap()
            .with_audio_cache(home.clone(), &json!({}));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let started = std::time::Instant::now();
        let path = retry_media(|| async {
            let response = client.get(format!("{base}/retry")).send().await?;
            adapter.cache_attachment(response, ".ogg", false).await
        })
        .await
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(4500));
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"OggSfixture");
        assert_eq!(counts.lock().unwrap()["/retry"], 3);
        for suffix in ["denied", "html"] {
            assert!(retry_media(|| async {
                let response = client.get(format!("{base}/{suffix}")).send().await?;
                adapter.cache_attachment(response, ".ogg", false).await
            })
            .await
            .is_err());
            assert_eq!(counts.lock().unwrap()[&format!("/{suffix}")], 1);
        }
        let impatient = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let error =
            retry_media(|| async { Ok(impatient.get(format!("{base}/slow")).send().await?) })
                .await
                .unwrap_err();
        assert!(error.downcast_ref::<reqwest::Error>().unwrap().is_timeout());
        assert_eq!(
            counts.lock().unwrap()["/slow"],
            3,
            "timeout retries stop at the third attempt"
        );
        server.abort();
        tokio::fs::remove_dir_all(home).await.unwrap();
    }

    #[tokio::test]
    async fn media_cookies_survive_redirects_and_retries_with_url_scoping() {
        use axum::{
            http::{HeaderMap, StatusCode, Uri},
            response::IntoResponse,
            routing::get,
            Router,
        };
        use reqwest::cookie::CookieStore;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app = Router::new().fallback(get(move |uri: Uri, headers: HeaderMap| {
            let calls = recorded.clone();
            async move {
                let cookie = headers
                    .get("cookie")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                calls
                    .lock()
                    .unwrap()
                    .push((uri.path().to_owned(), cookie.clone()));
                match uri.path() {
                    "/start" => {
                        let mut response =
                            (StatusCode::FOUND, [("location", "/final")]).into_response();
                        for value in [
                            "session=ok; Path=/",
                            "scoped=only; Path=/scoped",
                            "secure=only; Path=/; Secure",
                        ] {
                            response
                                .headers_mut()
                                .append("set-cookie", axum::http::HeaderValue::from_static(value));
                        }
                        response
                    }
                    "/cross" => (
                        StatusCode::FOUND,
                        [(
                            "location",
                            format!("http://cdn.example:{}/final", address.port()),
                        )],
                    )
                        .into_response(),
                    "/retry" if !cookie.contains("retry=ok") => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("set-cookie", "retry=ok; Path=/retry")],
                    )
                        .into_response(),
                    _ => "OggSfixture".into_response(),
                }
            }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("files.slack.com", address)
            .resolve("cdn.example", address)
            .build()
            .unwrap();
        let clients = |_url: reqwest::Url| {
            let client = client.clone();
            async move { Ok(client) }
        };
        let url = |path: &str| {
            format!("http://files.slack.com:{}{path}", address.port())
                .parse()
                .unwrap()
        };
        let jar = reqwest::cookie::Jar::default();
        let response = follow_media_redirects(url("/start"), "fixture".into(), &jar, clients)
            .await
            .unwrap();
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"OggSfixture");
        {
            let calls = calls.lock().unwrap();
            assert!(calls[0].1.is_empty());
            assert!(calls[1].1.contains("session=ok"));
            assert!(!calls[1].1.contains("scoped="));
            assert!(!calls[1].1.contains("secure="));
        }
        assert!(jar
            .cookies(&url("/scoped/file"))
            .unwrap()
            .to_str()
            .unwrap()
            .contains("scoped=only"));
        follow_media_redirects(url("/cross"), "fixture".into(), &jar, clients)
            .await
            .unwrap();
        assert!(
            calls.lock().unwrap().last().unwrap().1.is_empty(),
            "host-only cookies stay off other origins"
        );
        follow_media_redirects(
            url("/final"),
            "fixture".into(),
            &reqwest::cookie::Jar::default(),
            clients,
        )
        .await
        .unwrap();
        assert!(
            calls.lock().unwrap().last().unwrap().1.is_empty(),
            "new downloads start without previous cookies"
        );
        retry_media(|| async {
            Ok(
                follow_media_redirects(url("/retry"), "fixture".into(), &jar, clients)
                    .await?
                    .error_for_status()?,
            )
        })
        .await
        .unwrap();
        let requests = calls.lock().unwrap();
        let retries: Vec<_> = requests
            .iter()
            .filter(|(path, _)| path == "/retry")
            .collect();
        assert_eq!(retries.len(), 2);
        assert!(!retries[0].1.contains("retry=ok"));
        assert!(retries[1].1.contains("retry=ok"));
        server.abort();
    }

    #[tokio::test]
    async fn channel_controls_match_python_and_distinguish_group_dms() {
        let rows: Value = serde_json::from_str(include_str!(
            "../../../tools/slack-channel-policy-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let config = json!({"platforms":{"slack":{"extra":{"disable_dms":row["extra"], "allowed_channels":row["extra"]}}}});
            assert_eq!(
                disable_dms(&config, row["legacy"].as_str()),
                row["disabled"].as_bool().unwrap(),
                "{row}"
            );
            assert_eq!(
                json!(allowed_channels(&config, row["legacy"].as_str())),
                row["allowed"],
                "{row}"
            );
        }
        assert!(!disable_dms(&json!({"slack":{"disable_dms":2}}), None));
        assert!(disable_dms(&json!({"slack":{"disable_dms":true}}), None));
        let mut adapter = SlackAdapter::new("app", "bot").unwrap();
        adapter.team_tokens.lock().unwrap().primary_bot_user_id = "BOT".into();
        adapter.allowed_channels = ["C1".into()].into_iter().collect();
        adapter.disable_dms = false;
        let mut payload =
            json!({"event":{"type":"message","channel":"D1","user":"U1","text":"hello"}});
        assert_eq!(
            adapter
                .prepare_message(&payload)
                .await
                .unwrap()
                .chat_type
                .as_deref(),
            Some("dm")
        );
        payload["event"]["channel_type"] = json!("mpim");
        assert!(
            adapter.prepare_message(&payload).await.is_none(),
            "group DMs obey channel allowlists"
        );
        adapter.allowed_channels.insert("D1".into());
        assert_eq!(
            adapter
                .prepare_message(&payload)
                .await
                .unwrap()
                .chat_type
                .as_deref(),
            Some("dm")
        );
        adapter.disable_dms = true;
        assert!(adapter.prepare_message(&payload).await.is_none());
        payload["event"]["channel_type"] = json!("im");
        assert!(adapter.prepare_message(&payload).await.is_none());
        payload["event"]["channel"] = json!("C1");
        payload["event"]["channel_type"] = json!("channel");
        assert_eq!(
            adapter
                .prepare_message(&payload)
                .await
                .unwrap()
                .chat_type
                .as_deref(),
            Some("group")
        );
    }

    #[tokio::test]
    async fn thread_selection_matches_python_and_separates_history() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-thread-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            let config = json!({"platforms":{"slack":{"extra":row["extra"]}}});
            let adapter = SlackAdapter::new("app", "bot")
                .unwrap()
                .with_audio_cache(std::env::temp_dir(), &config);
            let thread = adapter.session_thread(&row["event"]);
            assert_eq!(json!(thread), row["selected"], "{row}");
            let mut message = parse_message_event(
                &json!({"event":{"type":"message","channel":"C1","user":"U1","text":"hello"}}),
            )
            .unwrap();
            message.message_id = Some("1".into());
            message.thread_id = thread;
            assert_eq!(
                json!(adapter.reply_thread(&message)),
                row["target"],
                "{row}"
            );
        }
        let home = std::env::temp_dir().join(format!(
            "slack-thread-{}",
            crate::install_identity::mint_id().unwrap()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let db = crate::session_db::SessionDb::open(home.join("state.db")).unwrap();
        let adapter = SlackAdapter::new("app", "bot").unwrap();
        let event = |ts: &str, thread: &str| json!({"team_id":"T1","event":{"type":"message","channel":"C1","user":"U1","text":"hello","ts":ts,"thread_ts":thread}});
        let root = adapter.prepare_message(&event("1", "")).await.unwrap();
        assert_eq!(root.thread_id.as_deref(), Some("1"));
        assert!(crate::session_db::begin_turn(Some(&db), false, &root, "slack").is_empty());
        crate::session_db::end_turn(Some(&db), false, &root, "reply");
        let other = adapter.prepare_message(&event("2", "")).await.unwrap();
        assert!(crate::session_db::begin_turn(Some(&db), false, &other, "slack").is_empty());
        let child = adapter.prepare_message(&event("3", "1")).await.unwrap();
        assert_eq!(adapter.reply_thread(&child), Some("1"));
        assert_eq!(
            crate::session_db::begin_turn(Some(&db), false, &child, "slack").len(),
            2
        );
        drop(db);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn thread_keys_match_python_store_configuration() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-thread-key-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let config = crate::config_gateway::GatewayConfig {
                group_sessions_per_user: row["group"].as_bool().unwrap(),
                thread_sessions_per_user: row["per_thread"].as_bool().unwrap(),
                ..Default::default()
            };
            assert_eq!(
                json!(thread_session_key(
                    &row["event"],
                    row["team"].as_str().unwrap(),
                    Some(&config),
                    row["profile"].as_str()
                )),
                row["result"],
                "{row}"
            );
        }
        assert_eq!(thread_session_key(&json!({}), "", None, None), None);
    }

    #[tokio::test]
    async fn thread_wake_fetches_roots_and_reuses_mentioned_memory() {
        use axum::{extract::Query, routing::get, Json, Router};
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = count.clone();
        let app = Router::new().route("/conversations.replies", get(move |Query(query): Query<std::collections::HashMap<String, String>>| {
            let captured = captured.clone();
            async move {
                captured.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let thread = &query["ts"];
                let messages = if thread == "missing" { vec![] } else { vec![json!({"ts":thread, "user":if thread == "own" {"BOT"} else {"HUMAN"}, "text":if thread == "mentioned" {"<@BOT> root"} else {"root"}})] };
                Json(json!({"ok":true,"messages":messages}))
            }
        })).route("/users.info", get(|| async { Json(json!({"ok":true,"user":{"name":"Name"}})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut adapter = SlackAdapter::new("app", "bot").unwrap();
        adapter.api_base = format!("http://{}", listener.local_addr().unwrap());
        adapter.team_tokens.lock().unwrap().teams.insert(
            "T1".into(),
            WorkspaceCredential {
                token: "token".into(),
                bot_user_id: "BOT".into(),
            },
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut request = ThreadRequest {
            channel: "C1",
            thread: "own",
            current: "child",
            team: "T1",
            after: "",
            limit: 30,
            force: false,
        };
        assert!(
            adapter
                .should_wake_on_unmentioned(&request, &|| false, &|_| Some(true))
                .await
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        request.thread = "mentioned";
        assert!(
            adapter
                .should_wake_on_unmentioned(&request, &|| false, &|_| Some(true))
                .await
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            adapter
                .should_wake_on_unmentioned(
                    &request,
                    &|| panic!("memory must short-circuit session probe"),
                    &|_| None
                )
                .await
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        request.team = "T2";
        assert!(
            adapter
                .should_wake_on_unmentioned(&request, &|| false, &|_| None)
                .await,
            "source parent-mention registration uses the legacy marker"
        );
        request.team = "T1";
        request.thread = "ordinary";
        assert!(
            !adapter
                .should_wake_on_unmentioned(&request, &|| false, &|_| Some(true))
                .await
        );
        request.thread = "missing";
        assert!(
            !adapter
                .should_wake_on_unmentioned(&request, &|| false, &|_| Some(true))
                .await
        );
        let before = count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            adapter
                .should_wake_on_unmentioned(&request, &|| true, &|_| None)
                .await
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), before);
        server.abort();
    }

    #[tokio::test]
    async fn thread_wake_matches_python_with_cached_parent_evidence() {
        let rows: Value = serde_json::from_str(include_str!(
            "../../../tools/slack-thread-wake-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let adapter = SlackAdapter::new("app", "bot").unwrap();
            adapter.team_tokens.lock().unwrap().teams.insert(
                "T1".into(),
                WorkspaceCredential {
                    token: "unused".into(),
                    bot_user_id: "BOT".into(),
                },
            );
            let thread = row["thread"].as_str().unwrap();
            {
                let mut markers = adapter.thread_markers.lock().unwrap();
                if row["sent"] == true {
                    markers.bot.insert(("T1".into(), thread.into()));
                }
                if row["mentioned"] == true {
                    markers.mentioned.insert(("T1".into(), thread.into()));
                }
            }
            adapter.thread_context.lock().unwrap().push_back((format!("C1:{thread}:T1"), ThreadContextEntry {
                content:String::new(), parent_text:"root".into(), parent_user:if row["authored"] == true {"BOT"} else {"HUMAN"}.into(),
                messages:vec![json!({"ts":thread,"text":if row["parent_mention"] == true {"<@BOT> root"} else {"root"}})], fetched:std::time::Instant::now(),
            }));
            let called = std::sync::atomic::AtomicBool::new(false);
            let active = || {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                row["active"] == true
            };
            let request = ThreadRequest {
                channel: "C1",
                thread,
                current: "2",
                team: "T1",
                after: "",
                limit: 30,
                force: false,
            };
            assert_eq!(
                adapter
                    .should_wake_on_unmentioned(&request, &active, &|_| None)
                    .await,
                row["result"].as_bool().unwrap(),
                "{row}"
            );
            assert_eq!(
                called.load(std::sync::atomic::Ordering::SeqCst),
                row["active_called"].as_bool().unwrap(),
                "{row}"
            );
            let registered = adapter
                .thread_markers
                .lock()
                .unwrap()
                .mentioned
                .iter()
                .filter(|(team, _)| team.is_empty())
                .map(|(_, ts)| ts.clone())
                .collect::<Vec<_>>();
            assert_eq!(json!(registered), row["registered"], "{row}");
        }
    }

    #[tokio::test]
    async fn thread_context_cache_keeps_full_text_and_reformats_deltas() {
        use axum::{extract::Query, http::HeaderMap, routing::get, Json, Router};
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = calls.clone();
        let replies = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = replies.clone();
        let app = Router::new()
            .route("/conversations.replies", get(move |Query(query): Query<std::collections::HashMap<String, String>>, headers: HeaderMap| {
                let calls = captured.clone(); let counter = counter.clone();
                async move {
                    let retry = query["ts"] == "retry";
                    calls.lock().unwrap().push((query, headers["authorization"].to_str().unwrap().to_owned()));
                    let count = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    let status = if retry && count < 6 { axum::http::StatusCode::TOO_MANY_REQUESTS } else { axum::http::StatusCode::OK };
                    (status, Json(json!({"ok":true,"messages":[{"ts":"1","user":"HUMAN","text":"<@BOT> root"},{"ts":"2","user":"HUMAN","text":format!("reply{count}\nline")},{"ts":"3","user":"HUMAN","text":"current"}]})))
                }
            }))
            .route("/users.info", get(|| async { Json(json!({"ok":true,"user":{"profile":{"display_name":"Name\nheading"}}})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut adapter = SlackAdapter::new("app", "primary").unwrap();
        adapter.api_base = format!("http://{}", listener.local_addr().unwrap());
        adapter.team_tokens.lock().unwrap().teams.insert(
            "T1".into(),
            WorkspaceCredential {
                token: "team-token".into(),
                bot_user_id: "BOT".into(),
            },
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut request = ThreadRequest {
            channel: "C1",
            thread: "1",
            current: "3",
            team: "T1",
            after: "",
            limit: 30,
            force: false,
        };
        let authorized = |_: &str| Some(false);
        let full = adapter.fetch_thread_context(&request, &authorized).await;
        assert!(full.contains("[thread parent] [unverified] Name heading: root"));
        assert!(full.contains("Name heading: reply1 line"));
        assert!(!full.contains(": current"));
        assert_eq!(
            adapter.fetch_thread_context(&request, &authorized).await,
            full
        );
        assert_eq!(
            adapter
                .fetch_thread_parent_text("C1", "1", "T1", false)
                .await,
            "<@BOT> root"
        );
        assert_eq!(
            adapter
                .fetch_thread_parent_text("C1", "1", "T1", true)
                .await,
            "root"
        );
        request.after = "1";
        let delta = adapter.fetch_thread_context(&request, &authorized).await;
        assert!(!delta.contains("[thread parent]"));
        assert!(delta.contains("reply1 line"));
        assert_eq!(replies.load(std::sync::atomic::Ordering::SeqCst), 1);
        request.force = true;
        assert!(adapter
            .fetch_thread_context(&request, &authorized)
            .await
            .contains("reply2 line"));
        request.force = false;
        request.after = "";
        let full = adapter.fetch_thread_context(&request, &authorized).await;
        assert!(full.contains("[thread parent]"));
        assert!(full.contains("reply2 line"));
        {
            let mut cache = adapter.thread_context.lock().unwrap();
            assert_eq!(cache[0].1.parent_user, "HUMAN");
            cache[0].1.fetched = std::time::Instant::now() - Duration::from_secs(61);
        }
        assert!(adapter
            .fetch_thread_context(&request, &authorized)
            .await
            .contains("reply3 line"));
        request.thread = "retry";
        assert!(adapter
            .fetch_thread_context(&request, &authorized)
            .await
            .contains("reply6 line"));
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 6);
        assert!(calls.iter().all(|(query, token)| query["limit"] == "31"
            && query["inclusive"] == "true"
            && token == "Bearer team-token"));
        server.abort();
    }

    #[tokio::test]
    async fn user_names_share_bot_results_and_preserve_workspace_scope() {
        use axum::{
            extract::Query,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
            Json, Router,
        };
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = calls.clone();
        let app = Router::new().route("/users.info", get(move |Query(query): Query<std::collections::HashMap<String, String>>, headers: HeaderMap| {
            let captured = captured.clone();
            async move {
                let token = headers["authorization"].to_str().unwrap().to_owned();
                captured.lock().unwrap().push((query["user"].clone(), token.clone()));
                let info = match query["user"].as_str() {
                    "DISPLAY" => json!({"is_bot":true,"profile":{"display_name":token,"real_name":"real"},"real_name":"outer","name":"handle"}),
                    "REAL" => json!({"profile":{"display_name":"","real_name":"real"},"real_name":"outer","name":"handle"}),
                    "OUTER" => json!({"real_name":"outer","name":"handle"}),
                    "HANDLE" => json!({"name":"handle"}),
                    "FAIL" => return (StatusCode::SERVICE_UNAVAILABLE, "failed").into_response(),
                    _ => json!({}),
                };
                Json(json!({"ok":true,"user":info})).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut adapter = SlackAdapter::new("app", "primary").unwrap();
        adapter.api_base = format!("http://{}", listener.local_addr().unwrap());
        for team in ["T1", "T2"] {
            adapter.team_tokens.lock().unwrap().teams.insert(
                team.into(),
                WorkspaceCredential {
                    token: team.into(),
                    bot_user_id: "BOT".into(),
                },
            );
        }
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        assert_eq!(
            adapter.resolve_user_name("T1", "C1", "DISPLAY").await,
            "Bearer T1"
        );
        assert!(adapter.resolve_user_is_bot("T1", "DISPLAY").await);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(adapter.resolve_user_is_bot("T2", "DISPLAY").await);
        assert_eq!(
            adapter.resolve_user_name("T2", "C1", "DISPLAY").await,
            "Bearer T2"
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
        for (user, name) in [
            ("REAL", "real"),
            ("OUTER", "outer"),
            ("HANDLE", "handle"),
            ("UNKNOWN", "UNKNOWN"),
        ] {
            assert_eq!(adapter.resolve_user_name("T1", "C1", user).await, name);
        }
        adapter
            .user_bots
            .lock()
            .unwrap()
            .push_back((("T1".into(), "FAIL".into()), true));
        assert_eq!(adapter.resolve_user_name("T1", "C1", "FAIL").await, "FAIL");
        let count = calls.lock().unwrap().len();
        assert_eq!(adapter.resolve_user_name("T1", "C1", "FAIL").await, "FAIL");
        assert!(adapter.resolve_user_is_bot("T1", "FAIL").await);
        assert_eq!(calls.lock().unwrap().len(), count);
        adapter
            .channel_teams
            .lock()
            .unwrap()
            .remember("C1", "T2", 10000);
        assert_eq!(
            adapter.resolve_user_name("", "C1", "DISPLAY").await,
            "Bearer T2"
        );
        assert_eq!(
            adapter.resolve_user_name("T1", "", "PRIMARY").await,
            "PRIMARY"
        );
        assert_eq!(calls.lock().unwrap().last().unwrap().1, "Bearer primary");
        assert_eq!(adapter.resolve_user_name("T1", "C1", "").await, "");
        server.abort();
    }

    #[tokio::test]
    async fn cold_parent_lookup_uses_workspace_token_and_validates_root() {
        use axum::{
            extract::Query,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
            Json, Router,
        };
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = calls.clone();
        let app = Router::new().route("/conversations.replies", get(move |Query(query): Query<std::collections::HashMap<String, String>>, headers: HeaderMap| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push((query.clone(), headers.get("authorization").unwrap().to_str().unwrap().to_owned()));
                match query["ts"].as_str() {
                    "failed" => (StatusCode::SERVICE_UNAVAILABLE, "failed").into_response(),
                    "denied" => Json(json!({"ok":false,"messages":[{"ts":"denied","text":"must not leak"}]})).into_response(),
                    "wrong" => Json(json!({"ok":true,"messages":[{"ts":"different","text":"wrong root"}]})).into_response(),
                    "empty" => Json(json!({"ok":true,"messages":[]})).into_response(),
                    _ => Json(json!({"ok":true,"messages":[{"ts":"1","text":"<@BOT> hello","blocks":[{"type":"section","text":{"text":"Alert"},"accessory":{"url":"https://example.com"}}],"files":[{"name":"chart.png","mimetype":"image/png"}]}]})).into_response(),
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut adapter = SlackAdapter::new("app", "primary-token").unwrap();
        adapter.api_base = format!("http://{}", listener.local_addr().unwrap());
        adapter.team_tokens.lock().unwrap().teams.insert(
            "T1".into(),
            WorkspaceCredential {
                token: "team-token".into(),
                bot_user_id: "BOT".into(),
            },
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        for strip in [false, true] {
            assert_eq!(
                adapter
                    .fetch_thread_parent_text("C1", "1", "T1", strip)
                    .await,
                "hello\nAlert\nURLs: https://example.com\n[image: chart.png]"
            );
        }
        for thread in ["failed", "denied", "wrong", "empty"] {
            assert!(adapter
                .fetch_thread_parent_text("C1", thread, "T1", false)
                .await
                .is_empty());
        }
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 6);
        for (query, authorization) in recorded.iter() {
            assert_eq!(authorization, "Bearer team-token");
            assert_eq!(query["channel"], "C1");
            assert_eq!(query["limit"], "1");
            assert_eq!(query["inclusive"], "true");
        }
        server.abort();
    }

    #[tokio::test]
    async fn thread_markers_match_python_and_record_mentions_after_gates() {
        let rows: Value = serde_json::from_str(include_str!(
            "../../../tools/slack-thread-marker-goldens.json"
        ))
        .unwrap();
        let mut markers = ThreadMarkers::default();
        for row in rows.as_array().unwrap() {
            if row["reset"] == true {
                markers = ThreadMarkers::default();
            }
            let team = row["team"].as_str().unwrap();
            let ts = row["ts"].as_str().unwrap();
            markers.remember_mention(team, ts, 4);
            markers.remember_sent(team, ts, None, 4);
            assert_eq!(json!(markers.bot), row["bot"], "{row}");
            assert_eq!(json!(markers.mentioned), row["mentioned"], "{row}");
        }
        let mut adapter = SlackAdapter::new("app", "bot").unwrap();
        adapter.team_tokens.lock().unwrap().primary_bot_user_id = "BOT".into();
        let payload = |team: &str, ts: &str| json!({"team_id":team,"event":{"type":"message","channel":"C1","user":"HUMAN","text":"<@BOT> hello","ts":ts}});
        assert!(adapter.prepare_message(&payload("T1", "1")).await.is_some());
        assert!(adapter.prepare_message(&payload("T2", "1")).await.is_some());
        assert_eq!(adapter.thread_markers.lock().unwrap().mentioned.len(), 2);
        adapter.strict_mentions.strict = true;
        assert!(adapter.prepare_message(&payload("T1", "2")).await.is_some());
        assert_eq!(adapter.thread_markers.lock().unwrap().mentioned.len(), 2);
        adapter.strict_mentions.strict = false;
        adapter.ignored_channels.insert("C1".into());
        assert!(adapter.prepare_message(&payload("T1", "3")).await.is_none());
        assert_eq!(adapter.thread_markers.lock().unwrap().mentioned.len(), 2);
    }

    #[tokio::test]
    async fn outbound_thread_payload_uses_root_and_can_send_flat() {
        use axum::{routing::post, Json, Router};
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = bodies.clone();
        let app = Router::new().route(
            "/chat.postMessage",
            post(move |Json(body): Json<Value>| {
                let bodies = captured.clone();
                async move {
                    bodies.lock().unwrap().push(body);
                    Json(json!({"ok":true,"ts":"sent"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut adapter = SlackAdapter::new("app", "bot").unwrap();
        adapter.api_base = base;
        let mut message = parse_message_event(
            &json!({"event":{"type":"message","channel":"C1","user":"U1","text":"reply"}}),
        )
        .unwrap();
        message.message_id = Some("child".into());
        message.thread_id = Some("root".into());
        adapter.send(&message).await.unwrap();
        adapter.reply_in_thread = false;
        adapter.send(&message).await.unwrap();
        message.thread_id = message.message_id.clone();
        adapter.send(&message).await.unwrap();
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies[0]["thread_ts"], "root");
        assert_eq!(bodies[1]["thread_ts"], "root");
        assert!(bodies[2].get("thread_ts").is_none());
        assert!(bodies
            .iter()
            .all(|b| b["channel"] == "C1" && b["text"] == "reply"));
        let markers = adapter.thread_markers.lock().unwrap();
        assert_eq!(
            markers.bot,
            [
                (String::new(), "root".into()),
                (String::new(), "sent".into())
            ]
            .into_iter()
            .collect()
        );
        server.abort();
    }

    #[test]
    fn video_classification_matches_python() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-video-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            assert_eq!(json!(video_extension(&row["file"])), row["result"], "{row}");
        }
    }

    #[test]
    fn audio_classification_matches_python() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/slack-audio-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            assert_eq!(json!(audio_extension(&row["file"])), row["result"], "{row}");
        }
        for url in [
            "https://files.slack.com/a",
            "https://team.slack.com./a",
            "https://slack-files.com/a",
        ] {
            assert!(slack_file_url(url).is_ok());
        }
        for url in [
            "http://files.slack.com/a",
            "https://files.slack.com.evil.invalid/a",
            "https://user@files.slack.com/a",
            "https://evil.invalid/a",
            "file:///a",
        ] {
            assert!(slack_file_url(url).is_err());
        }
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.100.100.200",
            "169.254.169.254",
            "224.0.0.1",
            "::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:100.64.0.1",
        ] {
            assert!(!public_download_address(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_download_address(ip.parse().unwrap()));
        }
    }

    #[tokio::test]
    async fn private_audio_uses_bot_token_and_rejects_html_redirects_and_oversize() {
        use axum::{http::HeaderMap, routing::get, Router};
        let app = Router::new()
            .route(
                "/audio",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer bot-fixture");
                    b"1234ftypM4A fixture".to_vec()
                }),
            )
            .route(
                "/html",
                get(|| async { ([("content-type", "text/html")], "login") }),
            )
            .route(
                "/redirect",
                get(|| async {
                    (
                        axum::http::StatusCode::FOUND,
                        [("location", "https://evil.invalid/token")],
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let home = std::env::temp_dir().join(format!(
            "hermes-slack-audio-{}",
            crate::install_identity::mint_id().unwrap()
        ));
        let mut adapter = SlackAdapter::new("app-fixture", "bot-fixture")
            .unwrap()
            .with_audio_cache(home.clone(), &json!({}));
        // The HTTP effect is tested separately from production HTTPS/DNS checks;
        // production download_attachment constructs a pinned, validating client.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let path = adapter
            .fetch_attachment(
                "",
                &client,
                format!("{base}/audio").parse().unwrap(),
                ".ogg",
                false,
            )
            .await
            .unwrap();
        assert_eq!(path.extension().unwrap(), "m4a");
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"1234ftypM4A fixture");
        let video = adapter
            .fetch_attachment(
                "",
                &client,
                format!("{base}/audio").parse().unwrap(),
                ".mp4",
                true,
            )
            .await
            .unwrap();
        assert_eq!(video.extension().unwrap(), "mp4");
        assert_eq!(video.parent().unwrap(), home.join("cache/videos"));
        assert_eq!(
            tokio::fs::read(&video).await.unwrap(),
            b"1234ftypM4A fixture"
        );
        assert!(adapter
            .fetch_attachment(
                "",
                &client,
                format!("{base}/audio").parse().unwrap(),
                "../escape",
                true
            )
            .await
            .is_err());
        for suffix in ["html", "redirect"] {
            assert!(adapter
                .fetch_attachment(
                    "",
                    &client,
                    format!("{base}/{suffix}").parse().unwrap(),
                    ".ogg",
                    false
                )
                .await
                .is_err());
        }
        adapter.audio_limit = 3;
        assert!(adapter
            .fetch_attachment(
                "",
                &client,
                format!("{base}/audio").parse().unwrap(),
                ".ogg",
                false
            )
            .await
            .is_err());
        let payload = json!({"event":{"type":"message","subtype":"file_share","user":"U1","channel":"C1","text":"caption","files":[
            {"mimetype":"video/mp4","subtype":"slack_audio","url_private":"https://evil.invalid/secret"}
        ]}});
        let message = adapter.prepare_message(&payload).await.unwrap();
        assert!(message.audio_paths.is_empty());
        assert!(message.text.starts_with("caption\n"));
        assert!(!message.text.contains("secret"));
        let mut voice = payload.clone();
        voice["event"]["text"] = json!("");
        assert!(parse_message_event(&voice).is_some());
        voice["event"]["files"][0]["subtype"] = json!("slack_video");
        assert!(parse_message_event(&voice).is_some());
        let video_failure = adapter.prepare_message(&voice).await.unwrap();
        assert!(video_failure.video_paths.is_empty());
        assert_eq!(
            video_failure.text,
            "[Video attachment could not be downloaded.]"
        );
        voice["event"]["bot_id"] = json!("B1");
        assert!(adapter.prepare_message(&voice).await.is_none());
        server.abort();
        tokio::fs::remove_dir_all(home).await.unwrap();
    }

    #[test]
    fn parses_envelope_with_id() {
        let v = json!({
            "type": "events_api",
            "envelope_id": "abc",
            "payload": {"event": {"type": "message"}}
        });
        let env = parse_envelope(&v).unwrap();
        assert_eq!(env.envelope_id.as_deref(), Some("abc"));
        assert_eq!(env.kind, "events_api");
    }

    #[test]
    fn hello_has_no_envelope_id() {
        let env = parse_envelope(&json!({"type": "hello"})).unwrap();
        assert_eq!(env.kind, "hello");
        assert!(env.envelope_id.is_none());
    }

    #[test]
    fn ack_is_just_the_envelope_id() {
        assert_eq!(ack_payload("xyz"), json!({"envelope_id": "xyz"}));
    }

    #[test]
    fn parses_channel_message_as_group() {
        let payload = json!({"event": {
            "type": "message", "text": "hello",
            "channel": "C123", "user": "U456", "channel_type": "channel"
        }});
        let m = parse_message_event(&payload).unwrap();
        assert_eq!(m.platform, Platform::Slack);
        assert_eq!(m.channel_id, "C123");
        assert_eq!(m.sender_id, "U456");
        assert_eq!(m.text, "hello");
        assert_eq!(m.chat_type.as_deref(), Some("group"));
    }

    #[test]
    fn im_is_dm_scope() {
        let payload = json!({"event": {
            "type": "message", "text": "hi",
            "channel": "D1", "user": "U1", "channel_type": "im"
        }});
        assert_eq!(
            parse_message_event(&payload).unwrap().chat_type.as_deref(),
            Some("dm")
        );
    }

    #[test]
    fn skips_bot_and_subtype_and_nonmessage() {
        let bot = json!({"event": {"type": "message", "text": "x", "channel": "C", "user": "U", "bot_id": "B1"}});
        assert!(parse_message_event(&bot).is_none());
        let edited = json!({"event": {"type": "message", "subtype": "message_changed", "text": "x", "channel": "C", "user": "U"}});
        assert!(parse_message_event(&edited).is_none());
        let reaction = json!({"event": {"type": "reaction_added"}});
        assert!(parse_message_event(&reaction).is_none());
    }
}
