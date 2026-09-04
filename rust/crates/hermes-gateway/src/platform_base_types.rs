//! Port of the value types and send-error classifiers from gateway/platforms/base.py.
//!
// Public API is ahead of its callers while the platform adapters are ported.
#![allow(dead_code)]
//!
//! The self-contained slice of `gateway/platforms/base.py`: the incoming-message
//! value types (`MessageType`, `ProcessingOutcome`, `MessageEvent`), the
//! send-result type (`SendResult`), the platform-neutral send-error classifiers
//! (`classify_send_error`, `is_chat_level_not_found`, `error_blob` and the
//! substring/pattern tables they match against), and `EphemeralReply`.
//!
//! The big `BasePlatformAdapter` abstract base, the audio/TTS handles, and the
//! media caching (`CachedMedia`, `cache_media_bytes`) are NOT here: they are
//! coupled to the runner and adapter internals and land with the adapter
//! subsystem. `merge_pending_message_event` and `coerce_plaintext_gateway_command`
//! also stay behind because they reach into `BasePlatformAdapter._merge_caption`
//! and the runner's pending-event map.
//!
//! `MessageEvent.source` reuses `crate::session::SessionSource`. Python's
//! dataclass declares `source: SessionSource = None`, so it is modeled as an
//! `Option` to keep the None default faithful. `timestamp` defaults to
//! `Utc::now()`, mirroring Python's `datetime.now()` field factory.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::session::SessionSource;

/// Types of incoming messages (mirrors the Python `MessageType` enum values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Text,
    Location,
    Photo,
    Video,
    Audio,
    Voice,
    Document,
    Sticker,
    /// `/command` style.
    Command,
}

impl MessageType {
    /// Wire string value (matches the Python enum values exactly).
    pub fn value(self) -> &'static str {
        match self {
            MessageType::Text => "text",
            MessageType::Location => "location",
            MessageType::Photo => "photo",
            MessageType::Video => "video",
            MessageType::Audio => "audio",
            MessageType::Voice => "voice",
            MessageType::Document => "document",
            MessageType::Sticker => "sticker",
            MessageType::Command => "command",
        }
    }

    /// Parse from the wire string value; `None` for an unknown token.
    pub fn from_value(s: &str) -> Option<Self> {
        Some(match s {
            "text" => MessageType::Text,
            "location" => MessageType::Location,
            "photo" => MessageType::Photo,
            "video" => MessageType::Video,
            "audio" => MessageType::Audio,
            "voice" => MessageType::Voice,
            "document" => MessageType::Document,
            "sticker" => MessageType::Sticker,
            "command" => MessageType::Command,
            _ => return None,
        })
    }
}

/// Result classification for message-processing lifecycle hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingOutcome {
    Success,
    Failure,
    Cancelled,
}

impl ProcessingOutcome {
    /// Wire string value (matches the Python enum values exactly).
    pub fn value(self) -> &'static str {
        match self {
            ProcessingOutcome::Success => "success",
            ProcessingOutcome::Failure => "failure",
            ProcessingOutcome::Cancelled => "cancelled",
        }
    }

    /// Parse from the wire string value; `None` for an unknown token.
    pub fn from_value(s: &str) -> Option<Self> {
        Some(match s {
            "success" => ProcessingOutcome::Success,
            "failure" => ProcessingOutcome::Failure,
            "cancelled" => ProcessingOutcome::Cancelled,
            _ => return None,
        })
    }
}

/// Auto-loaded skill binding for a topic/channel. Python's `Optional[str |
/// list[str]]` allows a single name or an ordered list; the distinction is kept
/// here rather than collapsing a single name into a one-element list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoSkill {
    Single(String),
    List(Vec<String>),
}

/// Incoming message from a platform: the normalized representation every
/// adapter produces. Only `is_command`/`get_command`/`get_command_args` carry
/// behavior; the rest are carried fields consumed by later adapter ports.
#[derive(Debug, Clone)]
pub struct MessageEvent {
    // Message content.
    pub text: String,
    pub message_type: MessageType,

    // Author of this inbound message. Carried on the event itself (not only on
    // `source`) so per-message prompt builders can resolve "who said this"
    // without digging into `source`. Non-IM adapters (cron, webhook,
    // autonomous) may leave these as None.
    pub user_id: Option<String>,
    pub user_name: Option<String>,

    // Source information. Python declares `source: SessionSource = None`, so
    // this is an Option to keep the None default faithful.
    pub source: Option<SessionSource>,

    // Original platform data. Python's `raw_message: Any`; carried as JSON here.
    pub raw_message: Option<Value>,
    pub message_id: Option<String>,

    // Platform-specific update identifier (Telegram's `update_id`; other
    // platforms ignore it). Used by `/restart` to advance the Telegram offset.
    pub platform_update_id: Option<i64>,

    // Media attachments. `media_urls` are local file paths (for vision tool
    // access). `media_text_inlined` is the per-attachment inlining contract;
    // None/absent preserves the legacy assumption that text/* adapters already
    // injected content into `text`.
    pub media_urls: Vec<String>,
    pub media_types: Vec<String>,
    pub media_text_inlined: Vec<Option<bool>>,

    // Reply context.
    pub reply_to_message_id: Option<String>,
    pub reply_to_text: Option<String>,
    pub reply_to_author_id: Option<String>,
    pub reply_to_author_name: Option<String>,
    /// True when the user replied to this bot/assistant's own message.
    pub reply_to_is_own_message: bool,

    // Structured interactive-prompt reply (relay Phase 3). Present when this
    // event is the user answering a native interactive prompt rendered by the
    // relay connector. Shape mirrors the wire contract:
    // {prompt_id, option_id, label?, prompt_message_id?}.
    pub prompt_response: Option<Value>,

    // Auto-loaded skill(s) for topic/channel bindings.
    pub auto_skill: Option<AutoSkill>,

    // Per-channel ephemeral system prompt (applied at API call time, never
    // persisted to transcript history).
    pub channel_prompt: Option<String>,

    // Channel context recovered by history backfill. Kept separate from `text`
    // so the sender-prefix logic can operate on the trigger message alone.
    pub channel_context: Option<String>,

    // Internal flag: set for synthetic events (e.g. background process
    // completion notifications) that must bypass user authorization checks.
    pub internal: bool,

    // Free-form per-event metadata. Adapters set platform-specific signals here;
    // plugins must not rely on any particular key existing.
    pub metadata: Map<String, Value>,

    // Timestamp (Python's `datetime.now()` field factory -> `Utc::now()`).
    pub timestamp: DateTime<Utc>,

    // Whether this event may resolve gateway commands or pending control
    // prompts. Proactive plugin events set this to False so untrusted payload
    // text stays conversational input.
    pub allow_gateway_control: bool,
}

impl Default for MessageEvent {
    fn default() -> Self {
        Self {
            text: String::new(),
            message_type: MessageType::Text,
            user_id: None,
            user_name: None,
            source: None,
            raw_message: None,
            message_id: None,
            platform_update_id: None,
            media_urls: Vec::new(),
            media_types: Vec::new(),
            media_text_inlined: Vec::new(),
            reply_to_message_id: None,
            reply_to_text: None,
            reply_to_author_id: None,
            reply_to_author_name: None,
            reply_to_is_own_message: false,
            prompt_response: None,
            auto_skill: None,
            channel_prompt: None,
            channel_context: None,
            internal: false,
            metadata: Map::new(),
            timestamp: Utc::now(),
            allow_gateway_control: true,
        }
    }
}

impl MessageEvent {
    /// A text event with just its content set (all other fields defaulted).
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    /// Check if this is a command message (e.g. `/new`, `/reset`).
    ///
    /// Faithful to Python `allow_gateway_control and (self.text or "").lstrip()
    /// .startswith("/")`.
    pub fn is_command(&self) -> bool {
        self.allow_gateway_control && lstrip(&self.text).starts_with('/')
    }

    /// Extract the command name if this is a command message.
    ///
    /// Mirrors Python exactly: split off the first whitespace-delimited token,
    /// drop the leading `/`, lowercase it, cut at the first `@` (bot mention),
    /// and reject anything still containing `/` (never a valid command name).
    pub fn get_command(&self) -> Option<String> {
        if !self.is_command() {
            return None;
        }
        let command_text = lstrip(&self.text);
        let parts = split_whitespace_maxsplit1(command_text);
        // `parts` is never empty here (text starts with '/'), but mirror the
        // Python `parts[0][1:] if parts else None` guard for total fidelity.
        let first = match parts.first() {
            Some(f) => *f,
            None => return None,
        };
        // Drop the leading '/' (first char) and lowercase.
        let mut raw: String = first.chars().skip(1).collect::<String>().to_lowercase();
        if !raw.is_empty() && raw.contains('@') {
            raw = raw.split('@').next().unwrap_or("").to_string();
        }
        // Reject file paths: valid command names never contain '/'.
        if !raw.is_empty() && raw.contains('/') {
            return None;
        }
        Some(raw)
    }

    /// Get the arguments after a command.
    ///
    /// For a non-command event returns the raw text unchanged. Otherwise returns
    /// the remainder after the first whitespace run, with the iOS auto-correct
    /// dash normalization applied ("——" -> "--", "—" -> "--", "–" -> "-"), in
    /// that order.
    pub fn get_command_args(&self) -> String {
        if !self.is_command() {
            return self.text.clone();
        }
        let command_text = lstrip(&self.text);
        let parts = split_whitespace_maxsplit1(command_text);
        let args = if parts.len() > 1 { parts[1] } else { "" };
        // iOS auto-corrects -- to — (em dash) and - to – (en dash). Order is
        // load-bearing: collapse the double em-dash before the single one.
        args.replace("\u{2014}\u{2014}", "--")
            .replace('\u{2014}', "--")
            .replace('\u{2013}', "-")
    }
}

/// Mirror of Python `str.lstrip()` (no args): drop leading Unicode whitespace.
fn lstrip(s: &str) -> &str {
    s.trim_start_matches(char::is_whitespace)
}

/// Mirror of Python `str.split(maxsplit=1)` with the default (whitespace)
/// separator: at most two tokens, leading/inter-token whitespace collapsed, a
/// trailing whitespace-only run producing no second token.
fn split_whitespace_maxsplit1(s: &str) -> Vec<&str> {
    let head = s.trim_start_matches(char::is_whitespace);
    if head.is_empty() {
        return Vec::new();
    }
    match head.find(char::is_whitespace) {
        None => vec![head],
        Some(idx) => {
            let first = &head[..idx];
            let rest = head[idx..].trim_start_matches(char::is_whitespace);
            if rest.is_empty() {
                vec![first]
            } else {
                vec![first, rest]
            }
        }
    }
}

/// Result of sending a message. `success` gates the rest; on failure `error`
/// carries the human-readable detail and `error_kind` the machine-readable
/// category (one of [`SEND_ERROR_KINDS`], set via [`classify_send_error`]).
#[derive(Debug, Clone, Default)]
pub struct SendResult {
    pub success: bool,
    pub message_id: Option<String>,
    pub error: Option<String>,
    /// Adapter-specific metadata (Python's `raw_response: Any`).
    pub raw_response: Option<Value>,
    /// True for transient connection errors; base retries automatically.
    pub retryable: bool,
    /// Server-requested retry delay in seconds (e.g. Telegram FloodWait
    /// `retry_after`); honored instead of the default backoff when present.
    pub retry_after: Option<f64>,
    /// When an oversized payload was split across platform messages, `message_id`
    /// is the LAST visible id and these are the additional ids in send order.
    /// Empty for the common single-message case.
    pub continuation_message_ids: Vec<String>,
    /// Machine-readable failure category (set only when `success` is false).
    pub error_kind: Option<String>,
}

/// Machine-readable send-failure categories. Platform-neutral vocabulary every
/// adapter populates `SendResult.error_kind` from.
///
///   too_long      content exceeded the platform's per-message size cap.
///   bad_format    the platform rejected the markup/entities (parse error).
///   forbidden     the bot is blocked/kicked/lacks permission to post.
///   not_found     the target chat/thread/message no longer exists.
///   rate_limited  the platform throttled the send (flood control).
///   transient     a connection-level failure that is safe to retry.
///   unknown       classification did not match any known shape.
pub const SEND_ERROR_KINDS: [&str; 7] = [
    "too_long",
    "bad_format",
    "forbidden",
    "not_found",
    "rate_limited",
    "transient",
    "unknown",
];

// `not_found` substrings split by blast radius. A *chat-level* not_found means
// the chat/user/group itself is gone (the whole target is dead). A
// *thread/topic/message-level* not_found (a deleted forum topic, an edited-away
// message) leaves the parent chat reachable and must NOT mark the whole chat
// dead. `classify_send_error` collapses both into "not_found";
// `is_chat_level_not_found` recovers the distinction. See gateway.dead_targets.
const CHAT_LEVEL_NOT_FOUND_SUBSTRINGS: [&str; 1] = ["chat not found"];
const SUBCHAT_NOT_FOUND_SUBSTRINGS: [&str; 5] = [
    "message to edit not found",
    "message to reply not found",
    "thread not found",
    "topic_deleted",
    "message_id_invalid",
];

// Error substrings that indicate a transient *connection* failure worth
// retrying. "timeout" / "timed out" / "readtimeout" / "writetimeout" are
// intentionally excluded: a read/write timeout on a non-idempotent call means
// the request may have reached the server, so retrying risks duplicate delivery.
// "connecttimeout" is safe because the connection was never established.
const RETRYABLE_ERROR_PATTERNS: [&str; 9] = [
    "connecterror",
    "connectionerror",
    "connectionreset",
    "connectionrefused",
    "connecttimeout",
    "network",
    "broken pipe",
    "remotedisconnected",
    "eoferror",
];

/// A raised send exception, reduced to the two pieces the classifiers read:
/// `str(exc)` and `exc.__class__.__name__`. Rust has no exceptions, so callers
/// build this from whatever error they caught.
#[derive(Debug, Clone)]
pub struct SendException {
    /// `str(exc)`.
    pub message: String,
    /// `exc.__class__.__name__`.
    pub class_name: String,
}

impl SendException {
    pub fn new(message: impl Into<String>, class_name: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            class_name: class_name.into(),
        }
    }
}

/// Build the lowercased text blob both send-error classifiers match against.
///
/// Single source of truth so `classify_send_error` and `is_chat_level_not_found`
/// can never drift. Includes `str(exc)` (when non-empty) and the exception's
/// class name, plus any explicit `error_text`.
pub fn error_blob(exc: Option<&SendException>, error_text: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !error_text.is_empty() {
        parts.push(error_text);
    }
    if let Some(e) = exc {
        if !e.message.is_empty() {
            parts.push(&e.message);
        }
        parts.push(&e.class_name);
    }
    parts.join(" ").to_lowercase()
}

/// Map a send exception / error string to a [`SEND_ERROR_KINDS`] value.
///
/// Platform-neutral: matches the lowercased text of `exc` (and/or `error_text`)
/// against the substrings the major messaging APIs use. Conservative: anything
/// unrecognized returns `"unknown"`.
pub fn classify_send_error(exc: Option<&SendException>, error_text: &str) -> &'static str {
    let blob = error_blob(exc, error_text);
    if blob.trim().is_empty() {
        return "unknown";
    }
    if blob.contains("message_too_long")
        || blob.contains("too long")
        || blob.contains("message is too long")
    {
        return "too_long";
    }
    if blob.contains("can't parse entities")
        || blob.contains("cant parse entities")
        || blob.contains("can't find end")
        || blob.contains("unsupported start tag")
        || (blob.contains("entity") && blob.contains("parse"))
        || (blob.contains("bad request") && blob.contains("entit"))
    {
        return "bad_format";
    }
    if blob.contains("forbidden")
        || blob.contains("bot was blocked")
        || blob.contains("blocked by the user")
        || blob.contains("user is deactivated")
        || blob.contains("not enough rights")
        || blob.contains("have no rights")
        || blob.contains("not a member")
    {
        return "forbidden";
    }
    if CHAT_LEVEL_NOT_FOUND_SUBSTRINGS
        .iter()
        .any(|s| blob.contains(s))
        || SUBCHAT_NOT_FOUND_SUBSTRINGS
            .iter()
            .any(|s| blob.contains(s))
    {
        return "not_found";
    }
    if blob.contains("flood")
        || blob.contains("too many requests")
        || blob.contains("retry after")
        || blob.contains("rate limit")
    {
        return "rate_limited";
    }
    for pat in RETRYABLE_ERROR_PATTERNS {
        if blob.contains(pat) {
            return "transient";
        }
    }
    if blob.contains("connecttimeout") {
        return "transient";
    }
    "unknown"
}

/// Whether a `not_found` failure means the *whole chat* is gone.
///
/// `classify_send_error` collapses chat-level and thread/topic/message-level
/// not_found into the single `"not_found"` kind. Only the chat-level case should
/// mark a delivery target dead. When both a chat-level and a sub-chat marker are
/// present, the sub-chat reading wins (conservative: never kill a chat that may
/// still be reachable).
pub fn is_chat_level_not_found(exc: Option<&SendException>, error_text: &str) -> bool {
    let blob = error_blob(exc, error_text);
    if SUBCHAT_NOT_FOUND_SUBSTRINGS
        .iter()
        .any(|s| blob.contains(s))
    {
        return false;
    }
    CHAT_LEVEL_NOT_FOUND_SUBSTRINGS
        .iter()
        .any(|s| blob.contains(s))
}

/// System-notice reply that auto-deletes after a TTL.
///
/// Python subclasses `str` with a `ttl_seconds` attribute so the wrapper stays
/// transparent to anything treating handler returns as text. Rust models it as a
/// struct: `text` is the underlying string and `ttl_seconds` defaults to `None`
/// (the pipeline then uses the configured `display.ephemeral_system_ttl`; a
/// default of 0 disables auto-deletion). Platforms without `delete_message`
/// silently ignore the TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralReply {
    pub text: String,
    pub ttl_seconds: Option<i64>,
}

impl EphemeralReply {
    /// Wrap text with the default TTL (`None`), matching Python's
    /// `EphemeralReply(text)` where `ttl_seconds` defaults to None.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ttl_seconds: None,
        }
    }

    /// Wrap text with an explicit TTL.
    pub fn with_ttl(text: impl Into<String>, ttl_seconds: Option<i64>) -> Self {
        Self {
            text: text.into(),
            ttl_seconds,
        }
    }

    /// The underlying text (Python's `EphemeralReply.text` property).
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- enum wire values -------------------------------------------------

    #[test]
    fn message_type_values_roundtrip() {
        for (mt, v) in [
            (MessageType::Text, "text"),
            (MessageType::Location, "location"),
            (MessageType::Photo, "photo"),
            (MessageType::Video, "video"),
            (MessageType::Audio, "audio"),
            (MessageType::Voice, "voice"),
            (MessageType::Document, "document"),
            (MessageType::Sticker, "sticker"),
            (MessageType::Command, "command"),
        ] {
            assert_eq!(mt.value(), v);
            assert_eq!(MessageType::from_value(v), Some(mt));
        }
        assert_eq!(MessageType::from_value("nope"), None);
    }

    #[test]
    fn processing_outcome_values_roundtrip() {
        for (po, v) in [
            (ProcessingOutcome::Success, "success"),
            (ProcessingOutcome::Failure, "failure"),
            (ProcessingOutcome::Cancelled, "cancelled"),
        ] {
            assert_eq!(po.value(), v);
            assert_eq!(ProcessingOutcome::from_value(v), Some(po));
        }
        assert_eq!(ProcessingOutcome::from_value("nope"), None);
    }

    // --- MessageEvent command parsing (golden vectors from Python) --------

    fn ev(text: &str, allow: bool) -> MessageEvent {
        MessageEvent {
            allow_gateway_control: allow,
            ..MessageEvent::new(text)
        }
    }

    #[test]
    fn is_command_and_get_command_golden() {
        // ("/new", True) -> is_cmd True, cmd "new", args ""
        let e = ev("/new", true);
        assert!(e.is_command());
        assert_eq!(e.get_command().as_deref(), Some("new"));
        assert_eq!(e.get_command_args(), "");

        // ("  /reset now", True) -> cmd "reset", args "now"
        let e = ev("  /reset now", true);
        assert!(e.is_command());
        assert_eq!(e.get_command().as_deref(), Some("reset"));
        assert_eq!(e.get_command_args(), "now");

        // ("/help@mybot", True) -> cmd "help", args ""
        let e = ev("/help@mybot", true);
        assert_eq!(e.get_command().as_deref(), Some("help"));
        assert_eq!(e.get_command_args(), "");

        // ("/help@mybot arg1 arg2", True) -> cmd "help", args "arg1 arg2"
        let e = ev("/help@mybot arg1 arg2", true);
        assert_eq!(e.get_command().as_deref(), Some("help"));
        assert_eq!(e.get_command_args(), "arg1 arg2");

        // ("/foo/bar", True) -> cmd None (rejects '/'), args ""
        let e = ev("/foo/bar", true);
        assert!(e.is_command());
        assert_eq!(e.get_command(), None);
        assert_eq!(e.get_command_args(), "");

        // ("hello", True) -> not a command; get_command_args returns raw text.
        let e = ev("hello", true);
        assert!(!e.is_command());
        assert_eq!(e.get_command(), None);
        assert_eq!(e.get_command_args(), "hello");

        // ("/new", False) -> allow_gateway_control gates it off entirely.
        let e = ev("/new", false);
        assert!(!e.is_command());
        assert_eq!(e.get_command(), None);
        assert_eq!(e.get_command_args(), "/new");

        // ("/", True) -> command with empty name, args "".
        let e = ev("/", true);
        assert!(e.is_command());
        assert_eq!(e.get_command().as_deref(), Some(""));
        assert_eq!(e.get_command_args(), "");

        // Empty / whitespace-only text is not a command; args returns raw text.
        let e = ev("", true);
        assert!(!e.is_command());
        assert_eq!(e.get_command(), None);
        assert_eq!(e.get_command_args(), "");
        let e = ev("   ", true);
        assert!(!e.is_command());
        assert_eq!(e.get_command_args(), "   ");
    }

    #[test]
    fn get_command_args_ios_dash_normalization() {
        // "/model set ——verbose —x –y" -> cmd "model", args "set --verbose --x -y".
        let e = ev(
            "/model set \u{2014}\u{2014}verbose \u{2014}x \u{2013}y",
            true,
        );
        assert_eq!(e.get_command().as_deref(), Some("model"));
        assert_eq!(e.get_command_args(), "set --verbose --x -y");
    }

    #[test]
    fn message_event_defaults() {
        let e = MessageEvent::new("hi");
        assert_eq!(e.text, "hi");
        assert_eq!(e.message_type, MessageType::Text);
        assert!(e.allow_gateway_control);
        assert!(e.source.is_none());
        assert!(e.media_urls.is_empty());
    }

    // --- send-error classifiers (golden vectors from Python) --------------

    #[test]
    fn classify_send_error_golden() {
        let cases: &[(&str, &str)] = &[
            ("Message is too long", "too_long"),
            ("MESSAGE_TOO_LONG", "too_long"),
            ("Bad Request: can't parse entities", "bad_format"),
            ("Bad Request: message has unsupported entity", "bad_format"),
            ("Forbidden: bot was blocked by the user", "forbidden"),
            ("user is deactivated", "forbidden"),
            ("Bad Request: chat not found", "not_found"),
            ("Bad Request: thread not found", "not_found"),
            ("topic_deleted", "not_found"),
            ("Too Many Requests: retry after 30", "rate_limited"),
            ("Flood control exceeded", "rate_limited"),
            ("ConnectionResetError", "transient"),
            ("network is unreachable", "transient"),
            ("broken pipe", "transient"),
            ("", "unknown"),
            ("something weird", "unknown"),
        ];
        for (text, expected) in cases {
            assert_eq!(
                classify_send_error(None, text),
                *expected,
                "classify_send_error(None, {text:?})"
            );
        }
    }

    #[test]
    fn classify_send_error_from_exception_class_name() {
        // str(exc) empty -> only the class name feeds the blob.
        let eof = SendException::new("", "EOFError");
        assert_eq!(classify_send_error(Some(&eof), ""), "transient");
        // str(exc) non-empty plus class name.
        let conn = SendException::new("boom", "ConnectionError");
        assert_eq!(classify_send_error(Some(&conn), ""), "transient");
        // A plain ValueError with an unrecognized message is unknown.
        let ve = SendException::new("nope", "ValueError");
        assert_eq!(classify_send_error(Some(&ve), ""), "unknown");
        // No exception and no text -> unknown.
        assert_eq!(classify_send_error(None, ""), "unknown");
    }

    #[test]
    fn is_chat_level_not_found_golden() {
        assert!(is_chat_level_not_found(None, "chat not found"));
        assert!(!is_chat_level_not_found(None, "thread not found"));
        // Sub-chat marker wins when both are present (conservative).
        assert!(!is_chat_level_not_found(
            None,
            "chat not found and thread not found"
        ));
        assert!(!is_chat_level_not_found(None, "message_id_invalid"));
        assert!(!is_chat_level_not_found(None, "random"));
    }

    #[test]
    fn error_blob_shape() {
        // error_text + str(exc) + class name, all lowercased and space-joined.
        let exc = SendException::new("Boom", "ConnectionError");
        assert_eq!(
            error_blob(Some(&exc), "Prefix"),
            "prefix boom connectionerror"
        );
        // Empty str(exc) is skipped, class name still included.
        let exc2 = SendException::new("", "EOFError");
        assert_eq!(error_blob(Some(&exc2), ""), "eoferror");
        assert_eq!(error_blob(None, ""), "");
    }

    #[test]
    fn send_error_kinds_contents() {
        // Every branch's return value is a member of the frozenset.
        for kind in [
            "too_long",
            "bad_format",
            "forbidden",
            "not_found",
            "rate_limited",
            "transient",
            "unknown",
        ] {
            assert!(SEND_ERROR_KINDS.contains(&kind));
        }
    }

    // --- EphemeralReply ---------------------------------------------------

    #[test]
    fn ephemeral_reply_defaults() {
        let r = EphemeralReply::new("gone soon");
        assert_eq!(r.text(), "gone soon");
        assert_eq!(r.ttl_seconds, None);
        let r2 = EphemeralReply::with_ttl("bye", Some(30));
        assert_eq!(r2.ttl_seconds, Some(30));
        assert_eq!(r2.text, "bye");
    }

    #[test]
    fn send_result_defaults() {
        let r = SendResult {
            success: true,
            ..Default::default()
        };
        assert!(r.success);
        assert!(!r.retryable);
        assert!(r.error_kind.is_none());
        assert!(r.continuation_message_ids.is_empty());
    }
}
