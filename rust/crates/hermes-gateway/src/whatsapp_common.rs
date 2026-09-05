//! Port of gateway/platforms/whatsapp_common.py.
//!
// Public API is ahead of its callers: the Baileys bridge adapter and the
// WhatsApp Cloud API adapter that mix this behavior in are not ported yet.
#![allow(dead_code)]
//!
//! Transport-agnostic WhatsApp behavior shared by the Baileys bridge adapter and
//! the official WhatsApp Cloud API adapter:
//!
//! - Allow-list / DM / group gating
//! - Mention detection (explicit @-mentions plus configurable regex patterns)
//! - Quoted-reply-to-bot detection
//! - Broadcast / Channel / Newsletter filtering
//! - WhatsApp-flavored markdown conversion
//! - Outgoing chunk length budgeting
//!
//! # How the mixin is modeled
//!
//! Python's `WhatsAppBehaviorMixin` owns no state. Its module docstring spells
//! out an explicit contract: the host adapter must set `self.config`,
//! `self.name`, `self._dm_policy`, `self._allow_from`, `self._group_policy`,
//! `self._group_allow_from`, `self._mention_patterns` and `self._reply_prefix`
//! before any mixin method runs, and the class attributes `MAX_MESSAGE_LENGTH`
//! and `DEFAULT_REPLY_PREFIX` may be overridden per adapter.
//!
//! Because that contract is closed and documented, every method in the mixin is
//! expressible without any un-ported adapter or runner internals. So this port
//! does BOTH of the following, deliberately:
//!
//!  1. Every method body lives in a free function that takes the values it
//!     needs as explicit parameters. That is where the logic and the tests are.
//!  2. [`WhatsAppBehavior`] is a trait whose required methods are exactly the
//!     Python attribute contract (plus `dm_allowlist_source`, which Python reads
//!     with `getattr(self, "_dm_allowlist_source", None)`), and whose provided
//!     methods are one-line delegations to those free functions. An adapter
//!     implements the eight accessors and gets the whole behavior layer, which
//!     is what `class Adapter(WhatsAppBehaviorMixin, BasePlatformAdapter)` buys
//!     in Python.
//!
//! The reason for the split rather than putting the bodies straight into the
//! trait: the free functions are testable on their own with no adapter in
//! existence, and callers outside an adapter (the CLI, the pairing path) can use
//! them directly the way Python calls the `@staticmethod`s off the class.
//!
//! `MAX_MESSAGE_LENGTH`, `SUPPORTS_CODE_BLOCKS` and `DEFAULT_REPLY_PREFIX` are
//! associated consts on the trait, mirroring the overridable class attributes.
//! That makes the trait non-object-safe, which is fine: adapters are concrete
//! types and Python resolves these off the instance's class anyway.
//!
//! NOTHING was deferred as adapter-coupled. `BasePlatformAdapter` is not ported,
//! but no mixin method reaches into it.
//!
//! # Divergences, all deliberate
//!
//!  * `resolve_whatsapp_bridge_dir` derives the install tree from
//!    `Path(__file__).resolve().parents[2]` in Python. Rust has no runtime
//!    source path, so [`install_root`] uses the compile-time `CARGO_MANIFEST_DIR`
//!    walked up three levels, the same trick `code_skew.rs` uses.
//!    [`resolve_whatsapp_bridge_dir_from`] takes both roots explicitly so the
//!    resolution order is testable without a read-only filesystem.
//!  * `_bot_ids_from_message` / `_message_mentions_bot` iterate whatever
//!    `data["botIds"]` holds. Python would iterate a bare string character by
//!    character and raise `TypeError` on a number; this port reproduces the list
//!    and string cases and treats every other shape as empty.
//!  * `_clean_bot_mention_text` iterates a Python `set`, whose order is
//!    arbitrary. This port iterates in sorted order so the result is
//!    deterministic. The substitutions are independent for any realistic bot-id
//!    set, so the output is the same.
//!  * `_compile_mention_patterns` uses `serde_json` where Python uses
//!    `json.loads`. `json.loads` additionally accepts `NaN`/`Infinity`; those
//!    would land in the not-a-list branch anyway.
//!  * `py_str` renders JSON arrays/objects as JSON rather than as Python
//!    `repr`. Only reachable from degenerate config shapes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fancy_regex::{Captures, Regex, RegexBuilder};
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::secret_scope::{get_secret, UnscopedSecretError};
use crate::whatsapp_identity::{
    expand_whatsapp_aliases_in_dir, get_whatsapp_session_dir, normalize_whatsapp_identifier,
};

// ---------------------------------------------------------------------------
// small Python-semantics helpers (kept private, per this crate's convention)
// ---------------------------------------------------------------------------

/// Python truthiness `bool(value)` for a JSON value.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(value)` for the shapes a config slot or a bridge payload actually holds.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// `type(value).__name__`, for the mention-patterns warning line.
fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "int"
            } else {
                "float"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `mapping.get(key)` with Python's "absent and present-but-None look the same"
/// behavior.
fn dget<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    data.get(key).filter(|v| !v.is_null())
}

/// `str(data.get(key) or "")`.
fn dget_str_or_empty(data: &Value, key: &str) -> String {
    match dget(data, key) {
        Some(v) if py_truthy(v) => py_str(v),
        _ => String::new(),
    }
}

/// The elements Python's `for candidate in data.get(key) or []` would yield.
/// A list yields its items; a non-empty string yields its characters; anything
/// else yields nothing (see the module-level divergence note).
fn dget_iterable(data: &Value, key: &str) -> Vec<Value> {
    match dget(data, key) {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// _get_wsecret
// ---------------------------------------------------------------------------

/// Scope-aware `WHATSAPP_*` read with the default-profile startup fallback.
/// Port of `_get_wsecret`.
///
/// Secondary profiles run under a profile runtime scope: the scope is
/// authoritative and a scoped miss returns `default` (no cross-profile borrow).
/// The DEFAULT profile's adapter constructs and sends *unscoped* under
/// multiplexing, where a bare `get_secret` fails closed with
/// [`UnscopedSecretError`] and would crash its WhatsApp path; there the process
/// environment holds that profile's own value, so fall back to it.
///
/// The Python `except UnscopedSecretError: val = os.getenv(name)` maps onto the
/// `Err` arm of [`crate::secret_scope::get_secret`]'s `Result`. Note that the
/// environment fallback does NOT apply the default, exactly as in Python: the
/// trailing `val if val is not None else default` is what supplies it.
pub fn get_wsecret(name: &str, default: Option<&str>) -> Option<String> {
    let val = match get_secret(name, default) {
        Ok(v) => v,
        Err(UnscopedSecretError { .. }) => std::env::var(name).ok(),
    };
    val.or_else(|| default.map(String::from))
}

// ---------------------------------------------------------------------------
// outbound text sanitation
// ---------------------------------------------------------------------------

/// `_OUTBOUND_INVISIBLE_CHARS_RE`: zero-width format characters that leak as
/// mojibake-looking prefixes in WhatsApp.
fn is_outbound_invisible(c: char) -> bool {
    matches!(c, '\u{200b}' | '\u{2060}' | '\u{2063}' | '\u{feff}')
}

/// `_OUTBOUND_ODD_SPACE_RE`: unicode spaces normalized to a plain ASCII space.
fn is_outbound_odd_space(c: char) -> bool {
    // The Python class is [\u00a0\u1680\u180e\u2000-\u200a\u202f\u205f\u3000];
    // the \u2000-\u200a run is split out so the range reads unambiguously.
    if ('\u{2000}'..='\u{200a}').contains(&c) {
        return true;
    }
    matches!(
        c,
        '\u{00a0}' | '\u{1680}' | '\u{180e}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Remove invisible formatting chars that leak badly in WhatsApp. Port of
/// `_sanitize_outbound_text`.
///
/// Some provider/gateway formatting paths emit unicode like WORD JOINER
/// (U+2060) plus NARROW NO-BREAK SPACE (U+202F). WhatsApp may render those as
/// mojibake-looking prefixes instead of invisible spacing. Normal text and emoji
/// joiners stay intact; known zero-width format chars are stripped and odd
/// unicode spaces are normalized.
///
/// The two Python character-class regexes are single-character classes, so this
/// is a straight per-character pass rather than two regex substitutions. The
/// `if not content: return content` guard is preserved by the empty-input early
/// return.
pub fn sanitize_outbound_text(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    content
        .chars()
        .filter(|c| !is_outbound_invisible(*c))
        .map(|c| if is_outbound_odd_space(c) { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------
// config-derived behavior
// ---------------------------------------------------------------------------

const TRUTHY_MENTION_STRINGS: &[&str] = &["true", "1", "yes", "on"];
const TRUTHY_ALLOW_ALL_STRINGS: &[&str] = &["true", "1", "yes"];

/// The prefix to add to outgoing replies in self-chat mode. Port of
/// `_effective_reply_prefix`.
///
/// `reply_prefix` is `self._reply_prefix`; `default_prefix` is the class
/// attribute `DEFAULT_REPLY_PREFIX` (overridable per adapter). An explicitly
/// configured prefix wins even when it is the empty string, matching Python's
/// `is not None` check.
///
/// Adapters with no self-chat concept (the Cloud API one) override the trait
/// method rather than calling this.
pub fn effective_reply_prefix(reply_prefix: Option<&str>, default_prefix: &str) -> String {
    // `_get_wsecret(..., default="self-chat") or "self-chat"` -- the trailing
    // `or` also rewrites an empty string, which the default alone would not.
    let whatsapp_mode = get_wsecret("WHATSAPP_MODE", Some("self-chat"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "self-chat".to_string());
    if whatsapp_mode != "self-chat" {
        return String::new();
    }
    if let Some(prefix) = reply_prefix {
        return prefix.replace("\\n", "\n");
    }
    if let Some(env_prefix) = get_wsecret("WHATSAPP_REPLY_PREFIX", None) {
        return env_prefix.replace("\\n", "\n");
    }
    default_prefix.to_string()
}

/// Reserve room for the reply prefix so the final message fits. Port of
/// `_outgoing_chunk_limit`.
///
/// `len(prefix)` is a Python code-point count, so this counts `chars()`. The
/// 1024 floor keeps enough space for `truncate_message`'s pagination indicator
/// and code-fence repair even if a very long prefix is configured, and it also
/// absorbs the negative value the subtraction can produce.
pub fn outgoing_chunk_limit(max_message_length: usize, prefix: &str) -> usize {
    let prefix_len = prefix.chars().count() as isize;
    let budget = max_message_length as isize - prefix_len;
    if budget < 1024 {
        1024
    } else {
        budget as usize
    }
}

/// Whether group messages require an explicit mention. Port of
/// `_whatsapp_require_mention`.
///
/// Note the asymmetry preserved from Python: the configured-string branch does
/// NOT strip whitespace before lowercasing, while the env branch has no
/// whitespace to strip because it compares the raw value.
pub fn whatsapp_require_mention(extra: &Map<String, Value>) -> bool {
    if let Some(configured) = extra.get("require_mention").filter(|v| !v.is_null()) {
        if let Value::String(s) = configured {
            return TRUTHY_MENTION_STRINGS.contains(&s.to_lowercase().as_str());
        }
        return py_truthy(configured);
    }
    let raw = get_wsecret("WHATSAPP_REQUIRE_MENTION", Some("false"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "false".to_string());
    TRUTHY_MENTION_STRINGS.contains(&raw.to_lowercase().as_str())
}

/// Chats that get a reply without needing a mention. Port of
/// `_whatsapp_free_response_chats`.
pub fn whatsapp_free_response_chats(extra: &Map<String, Value>) -> HashSet<String> {
    let raw = extra.get("free_response_chats").filter(|v| !v.is_null());
    match raw {
        Some(v) => split_allow_value(v),
        None => {
            let env = get_wsecret("WHATSAPP_FREE_RESPONSE_CHATS", Some("")).unwrap_or_default();
            split_csv(&env)
        }
    }
}

/// The shared `{part.strip() for part in ...}` body of
/// `_whatsapp_free_response_chats` and `_coerce_allow_list`.
fn split_allow_value(raw: &Value) -> HashSet<String> {
    match raw {
        Value::Array(items) => items
            .iter()
            .map(py_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        other => split_csv(&py_str(other)),
    }
}

fn split_csv(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Parse `allow_from` / `group_allow_from` from config or env var. Port of the
/// `_coerce_allow_list` staticmethod.
pub fn coerce_allow_list(raw: Option<&Value>) -> HashSet<String> {
    match raw.filter(|v| !v.is_null()) {
        None => HashSet::new(),
        Some(v) => split_allow_value(v),
    }
}

/// Allowlist currently enforced for DM intake / strict DM auth. Port of
/// `_live_dm_allow_from`.
///
/// `source` is `getattr(self, "_dm_allowlist_source", None)` and `allow_from` is
/// `self._allow_from`.
///
/// Source precedence matches construction: explicit config wins over any env
/// carrier. When the adapter was seeded from an env var, re-read that same key
/// so pairing approve/revoke takes effect without a restart (including an empty
/// value while the key is still present). When the key is absent -- a sole-entry
/// revoke calls `remove_env_value` -- the allowlist is empty rather than the
/// construction-time snapshot. Config-seeded adapters keep the in-memory
/// snapshot, which pairing revoke purges in place; a lower-precedence or stale
/// env value must not broaden access.
pub fn live_dm_allow_from(source: Option<&str>, allow_from: &HashSet<String>) -> HashSet<String> {
    if let Some(key) = source {
        if key != "config" {
            return match std::env::var(key) {
                Ok(val) => coerce_allow_list(Some(&Value::String(val))),
                // Key removed (e.g. sole-entry pairing revoke) -- do not revive
                // the stale construction snapshot.
                Err(_) => HashSet::new(),
            };
        }
    }
    allow_from.clone()
}

// ---------------------------------------------------------------------------
// JID helpers
// ---------------------------------------------------------------------------

/// Port of the `_normalize_whatsapp_id` staticmethod.
///
/// This is NOT `whatsapp_identity::normalize_whatsapp_identifier`. That one
/// strips a JID down to its bare numeric core; this one only collapses the
/// legacy `user:device@domain` shape by rewriting the FIRST colon into an `@`,
/// which for `user:device@s.whatsapp.net` yields `user@device@s.whatsapp.net`.
/// The bridge payload comparisons in this module are all done against ids that
/// went through this same transform, so the odd output is self-consistent.
pub fn normalize_whatsapp_id(value: Option<&str>) -> String {
    match value {
        None => String::new(),
        Some(v) => normalize_whatsapp_id_str(v),
    }
}

fn normalize_whatsapp_id_str(value: &str) -> String {
    // `if not value` -- an empty string is falsy in Python too.
    if value.is_empty() {
        return String::new();
    }
    let normalized = value.trim();
    if normalized.contains(':') && normalized.contains('@') {
        return normalized.replacen(':', "@", 1);
    }
    normalized.to_string()
}

/// `_normalize_whatsapp_id` over a raw JSON payload value, which is how
/// `_bot_ids_from_message` reaches it (`str(value).strip()` after a truthiness
/// guard).
pub fn normalize_whatsapp_id_value(value: &Value) -> String {
    if !py_truthy(value) {
        return String::new();
    }
    normalize_whatsapp_id_str(&py_str(value))
}

/// True for WhatsApp pseudo-chats that aren't real conversations. Port of
/// `_is_broadcast_chat`.
///
/// Covers Status updates (Stories) and Channel/Newsletter broadcasts. These show
/// up as inbound messages on Baileys but the agent should never reply: answering
/// a Story update spams the contact's status feed, and Channel posts aren't
/// addressable in the first place.
pub fn is_broadcast_chat(chat_id: &str) -> bool {
    if chat_id.is_empty() {
        return false;
    }
    let cid = chat_id.trim().to_lowercase();
    if cid == "status@broadcast" {
        return true;
    }
    // The @broadcast suffix covers status@broadcast plus any future
    // broadcast-list variants. @newsletter is the Channel JID suffix.
    cid.ends_with("@broadcast") || cid.ends_with("@newsletter")
}

// ---------------------------------------------------------------------------
// gating
// ---------------------------------------------------------------------------

/// Port of `_open_dm_opted_in`. The first read is a plain `os.getenv`, not a
/// scoped secret read, so it stays a plain env read here too.
pub fn open_dm_opted_in() -> bool {
    let gateway = std::env::var("GATEWAY_ALLOW_ALL_USERS").unwrap_or_default();
    if TRUTHY_ALLOW_ALL_STRINGS.contains(&gateway.to_lowercase().as_str()) {
        return true;
    }
    let scoped = get_wsecret("WHATSAPP_ALLOW_ALL_USERS", Some("")).unwrap_or_default();
    TRUTHY_ALLOW_ALL_STRINGS.contains(&scoped.to_lowercase().as_str())
}

/// Match a WhatsApp identifier against an allowlist across phone/LID forms,
/// resolving aliases through `session_dir`. Port of
/// `_matches_whatsapp_allowlist` with the bridge session directory injected.
///
/// WhatsApp delivers inbound senders in LID form (`<id>@lid`) while operators
/// usually configure allowlists with phone numbers, and vice versa, so a raw
/// set-membership check never matches a known contact. Both the candidate and
/// each allowlist entry resolve through the bridge's `lid-mapping-*.json` files
/// (the shared `whatsapp_identity` helper the gateway authz and session-key
/// paths already use) so either configured form resolves to the inbound form.
pub fn matches_whatsapp_allowlist_in_dir(
    candidate: &str,
    allow_from: &HashSet<String>,
    session_dir: &Path,
) -> bool {
    if allow_from.is_empty() {
        return false;
    }
    // Fast path: exact match against the raw configured value (e.g. a full
    // `@g.us` group JID or an entry that already matches verbatim).
    if allow_from.contains(candidate) {
        return true;
    }

    let candidate_aliases = expand_whatsapp_aliases_in_dir(candidate, session_dir);
    if candidate_aliases.is_empty() {
        return false;
    }
    for entry in allow_from {
        if entry == "*" {
            return true;
        }
        if candidate_aliases.contains(&normalize_whatsapp_identifier(entry)) {
            return true;
        }
        // The entry may itself be an unmapped form; expand it too so a phone
        // allowlist entry resolves when the inbound sender arrived as a LID.
        if !expand_whatsapp_aliases_in_dir(entry, session_dir).is_disjoint(&candidate_aliases) {
            return true;
        }
    }
    false
}

/// [`matches_whatsapp_allowlist_in_dir`] against the real bridge session
/// directory.
pub fn matches_whatsapp_allowlist(candidate: &str, allow_from: &HashSet<String>) -> bool {
    matches_whatsapp_allowlist_in_dir(candidate, allow_from, &get_whatsapp_session_dir())
}

// ---------------------------------------------------------------------------
// mention patterns
// ---------------------------------------------------------------------------

/// Compile the configured mention patterns. Port of `_compile_mention_patterns`.
///
/// `extra` is `self.config.extra`; `name` is `self.name` (log lines only).
///
/// Resolution: `extra["mention_patterns"]` wins; otherwise the
/// `WHATSAPP_MENTION_PATTERNS` secret is parsed as JSON, falling back to
/// newline-separated and then comma-separated splitting. A single string is
/// wrapped into a one-element list; anything that is neither a string nor a list
/// logs a warning and yields no patterns.
///
/// `re.IGNORECASE` maps to `RegexBuilder::case_insensitive(true)` rather than an
/// inline `(?i)` prefix, so a pattern carrying its own inline flags keeps its
/// meaning.
pub fn compile_mention_patterns(extra: &Map<String, Value>, name: &str) -> Vec<Regex> {
    let mut patterns: Option<Value> = extra
        .get("mention_patterns")
        .filter(|v| !v.is_null())
        .cloned();

    if patterns.is_none() {
        let raw = get_wsecret("WHATSAPP_MENTION_PATTERNS", Some("")).unwrap_or_default();
        let raw = raw.trim();
        if !raw.is_empty() {
            patterns = Some(match serde_json::from_str::<Value>(raw) {
                Ok(v) => v,
                Err(_) => {
                    let mut parts: Vec<Value> = raw
                        .lines()
                        .map(|p| p.trim())
                        .filter(|p| !p.is_empty())
                        .map(|p| Value::String(p.to_string()))
                        .collect();
                    if parts.is_empty() {
                        parts = raw
                            .split(',')
                            .map(|p| p.trim())
                            .filter(|p| !p.is_empty())
                            .map(|p| Value::String(p.to_string()))
                            .collect();
                    }
                    Value::Array(parts)
                }
            });
        }
    }

    let patterns = match patterns {
        None => return Vec::new(),
        Some(p) => p,
    };

    let items: Vec<Value> = match patterns {
        Value::String(s) => vec![Value::String(s)],
        Value::Array(a) => a,
        other => {
            warn!(
                "[{}] whatsapp mention_patterns must be a list or string; got {}",
                name,
                py_type_name(&other)
            );
            return Vec::new();
        }
    };

    let mut compiled: Vec<Regex> = Vec::new();
    for pattern in &items {
        let Value::String(pattern) = pattern else {
            continue;
        };
        if pattern.trim().is_empty() {
            continue;
        }
        match RegexBuilder::new(pattern).case_insensitive(true).build() {
            Ok(re) => compiled.push(re),
            Err(exc) => {
                warn!(
                    "[{}] Invalid WhatsApp mention pattern {:?}: {}",
                    name, pattern, exc
                );
            }
        }
    }
    if !compiled.is_empty() {
        info!(
            "[{}] Loaded {} WhatsApp mention pattern(s)",
            name,
            compiled.len()
        );
    }
    compiled
}

// ---------------------------------------------------------------------------
// message inspection
// ---------------------------------------------------------------------------

/// Port of `_bot_ids_from_message`.
pub fn bot_ids_from_message(data: &Value) -> HashSet<String> {
    let mut bot_ids = HashSet::new();
    for candidate in dget_iterable(data, "botIds") {
        let normalized = normalize_whatsapp_id_value(&candidate);
        if !normalized.is_empty() {
            bot_ids.insert(normalized);
        }
    }
    bot_ids
}

/// Port of `_message_is_reply_to_bot`.
pub fn message_is_reply_to_bot(data: &Value) -> bool {
    let quoted = match dget(data, "quotedParticipant") {
        Some(v) => normalize_whatsapp_id_value(v),
        None => String::new(),
    };
    if quoted.is_empty() {
        return false;
    }
    bot_ids_from_message(data).contains(&quoted)
}

/// Port of `_message_mentions_bot`.
///
/// Explicit `mentionedIds` intersection first, then a substring scan of the body
/// for either `@<bare id>` or the bare id on its own.
pub fn message_mentions_bot(data: &Value) -> bool {
    let bot_ids = bot_ids_from_message(data);
    if bot_ids.is_empty() {
        return false;
    }
    let mentioned_ids: HashSet<String> = dget_iterable(data, "mentionedIds")
        .iter()
        .map(normalize_whatsapp_id_value)
        .filter(|s| !s.is_empty())
        .collect();
    if !mentioned_ids.is_disjoint(&bot_ids) {
        return true;
    }

    let body = dget_str_or_empty(data, "body");
    let lower_body = body.to_lowercase();
    for bot_id in &bot_ids {
        let bare_id = bot_id.split('@').next().unwrap_or("").to_lowercase();
        if !bare_id.is_empty()
            && (lower_body.contains(&format!("@{bare_id}")) || lower_body.contains(&bare_id))
        {
            return true;
        }
    }
    false
}

/// Port of `_message_matches_mention_patterns`.
pub fn message_matches_mention_patterns(data: &Value, mention_patterns: &[Regex]) -> bool {
    if mention_patterns.is_empty() {
        return false;
    }
    let body = dget_str_or_empty(data, "body");
    mention_patterns
        .iter()
        .any(|p| p.is_match(&body).unwrap_or(false))
}

/// Port of `_clean_bot_mention_text`.
///
/// Strips a leading `@<bare bot id>` plus any trailing punctuation and spacing.
/// If the result is blank, the original text is returned unchanged.
///
/// Python iterates a `set` here, so the substitution order is arbitrary; this
/// port sorts for determinism (see the module divergence note).
pub fn clean_bot_mention_text(text: &str, data: &Value) -> String {
    if text.is_empty() {
        return String::new();
    }
    let bot_ids = bot_ids_from_message(data);
    let mut sorted: Vec<&String> = bot_ids.iter().collect();
    sorted.sort();

    let mut cleaned = text.to_string();
    for bot_id in sorted {
        let bare_id = bot_id.split('@').next().unwrap_or("");
        if bare_id.is_empty() {
            continue;
        }
        let pattern = format!(r"@{}\b[,:\-]*\s*", fancy_regex::escape(bare_id));
        let Ok(re) = Regex::new(&pattern) else {
            continue;
        };
        cleaned = re
            .replace_all(&cleaned, |_: &Captures| String::new())
            .into_owned();
    }
    let stripped = cleaned.trim();
    if stripped.is_empty() {
        text.to_string()
    } else {
        stripped.to_string()
    }
}

// ---------------------------------------------------------------------------
// formatting
// ---------------------------------------------------------------------------

fn fence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"```[\s\S]*?```").unwrap())
}

fn inline_code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`\n]+`").unwrap())
}

fn italic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?<!\*)\*(?!\s|\*)([^*\n]*?\S[^*\n]*?)\*(?!\*)").unwrap())
}

fn bold_star_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap())
}

fn bold_underscore_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"__(.+?)__").unwrap())
}

fn strike_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"~~(.+?)~~").unwrap())
}

fn header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap())
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap())
}

const FENCE_PH: &str = "\u{0}FENCE";
const CODE_PH: &str = "\u{0}CODE";

/// Convert standard markdown to WhatsApp-compatible formatting. Port of
/// `format_message`.
///
/// WhatsApp supports `*bold*`, `_italic_`, `~strikethrough~`, ```` ```code``` ````
/// and monospaced `` `inline` ``. Standard markdown uses different syntax for
/// bold/italic/strikethrough, so those are converted here. Fenced code blocks
/// and inline code are protected from conversion via placeholder substitution.
///
/// Every substitution uses a closure replacer rather than a `$1`-style template
/// string, because fancy-regex expands `$` in a template and Python's `\1`
/// templates do not, which would corrupt any `$` in the user's text.
pub fn format_message(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }

    let content = sanitize_outbound_text(content);

    // --- 1. Protect fenced code blocks from formatting changes ---
    let mut fences: Vec<String> = Vec::new();
    let result = fence_re()
        .replace_all(&content, |caps: &Captures| {
            fences.push(caps.get(0).unwrap().as_str().to_string());
            format!("{FENCE_PH}{}\u{0}", fences.len() - 1)
        })
        .into_owned();

    // --- 2. Protect inline code ---
    let mut codes: Vec<String> = Vec::new();
    let result = inline_code_re()
        .replace_all(&result, |caps: &Captures| {
            codes.push(caps.get(0).unwrap().as_str().to_string());
            format!("{CODE_PH}{}\u{0}", codes.len() - 1)
        })
        .into_owned();

    // --- 3. Convert markdown formatting to WhatsApp syntax ---
    // Italic: standard Markdown *text* -> WhatsApp _text_. This runs before the
    // bold conversion so **bold** does not become italic by accident. The
    // lookarounds avoid list bullets and bold delimiters.
    let result = italic_re()
        .replace_all(&result, |caps: &Captures| {
            format!("_{}_", caps.get(1).unwrap().as_str())
        })
        .into_owned();
    // Bold: **text** or __text__ -> *text*
    let result = bold_star_re()
        .replace_all(&result, |caps: &Captures| {
            format!("*{}*", caps.get(1).unwrap().as_str())
        })
        .into_owned();
    let result = bold_underscore_re()
        .replace_all(&result, |caps: &Captures| {
            format!("*{}*", caps.get(1).unwrap().as_str())
        })
        .into_owned();
    // Strikethrough: ~~text~~ -> ~text~
    let result = strike_re()
        .replace_all(&result, |caps: &Captures| {
            format!("~{}~", caps.get(1).unwrap().as_str())
        })
        .into_owned();
    // _text_ is already WhatsApp italic -- leave as-is.

    // --- 4. Convert markdown headers to bold text ---
    // # Header -> *Header*. Any *...* wrapping already produced by step 3 is
    // stripped (e.g. "# **Title**" -> "*Title*", not "**Title**", which WhatsApp
    // renders with literal asterisks). `len(inner) > 1` is a code-point count.
    let result = header_re()
        .replace_all(&result, |caps: &Captures| {
            let mut inner = caps.get(1).unwrap().as_str().trim().to_string();
            loop {
                let chars: Vec<char> = inner.chars().collect();
                if chars.len() > 1 && chars[0] == '*' && chars[chars.len() - 1] == '*' {
                    inner = chars[1..chars.len() - 1]
                        .iter()
                        .collect::<String>()
                        .trim()
                        .to_string();
                } else {
                    break;
                }
            }
            format!("*{inner}*")
        })
        .into_owned();

    // --- 5. Convert markdown links: [text](url) -> text (url) ---
    let mut result = link_re()
        .replace_all(&result, |caps: &Captures| {
            format!(
                "{} ({})",
                caps.get(1).unwrap().as_str(),
                caps.get(2).unwrap().as_str()
            )
        })
        .into_owned();

    // --- 6. Restore protected sections ---
    for (i, fence) in fences.iter().enumerate() {
        result = result.replace(&format!("{FENCE_PH}{i}\u{0}"), fence);
    }
    for (i, code) in codes.iter().enumerate() {
        result = result.replace(&format!("{CODE_PH}{i}\u{0}"), code);
    }

    result
}

// ---------------------------------------------------------------------------
// the mixin, as a trait
// ---------------------------------------------------------------------------

/// The behavior layer `WhatsAppBehaviorMixin` provides. Implement the required
/// accessors (the Python attribute contract) and the rest comes for free.
///
/// Transport-specific concerns (subprocess management, HTTP webhooks, Graph API
/// calls, media upload protocols) live in each adapter, not here.
pub trait WhatsAppBehavior {
    // -- class attributes (overridable per adapter) --

    /// WhatsApp message limit. A practical UX limit, not the protocol max:
    /// WhatsApp allows ~65K but long messages are unreadable on mobile.
    const MAX_MESSAGE_LENGTH: usize = 4096;

    /// WhatsApp renders fenced code blocks (monospace).
    const SUPPORTS_CODE_BLOCKS: bool = true;

    const DEFAULT_REPLY_PREFIX: &'static str = "⚕ *Hermes Agent*\n────────────\n";

    // -- the attribute contract the adapter's constructor must satisfy --

    /// `self.config.extra`
    fn config_extra(&self) -> &Map<String, Value>;
    /// `self.name` -- adapter name, used in log lines.
    fn adapter_name(&self) -> &str;
    /// `self._dm_policy` -- "open" | "allowlist" | "pairing" | "disabled"
    fn dm_policy(&self) -> &str;
    /// `self._allow_from`
    fn allow_from(&self) -> &HashSet<String>;
    /// `self._group_policy` -- "open" | "allowlist" | "pairing" | "disabled"
    fn group_policy(&self) -> &str;
    /// `self._group_allow_from`
    fn group_allow_from(&self) -> &HashSet<String>;
    /// `self._mention_patterns`
    fn mention_patterns(&self) -> &[Regex];
    /// `self._reply_prefix`
    fn reply_prefix(&self) -> Option<&str>;
    /// `getattr(self, "_dm_allowlist_source", None)` -- optional, hence the
    /// default. Adapters seeded from an env carrier return that key's name.
    fn dm_allowlist_source(&self) -> Option<&str> {
        None
    }

    // -- provided behavior --

    /// WhatsApp gates DM/group access at intake via dm_policy/group_policy.
    fn enforces_own_access_policy(&self) -> bool {
        true
    }

    fn effective_reply_prefix(&self) -> String
    where
        Self: Sized,
    {
        effective_reply_prefix(self.reply_prefix(), Self::DEFAULT_REPLY_PREFIX)
    }

    fn outgoing_chunk_limit(&self) -> usize
    where
        Self: Sized,
    {
        outgoing_chunk_limit(Self::MAX_MESSAGE_LENGTH, &self.effective_reply_prefix())
    }

    fn whatsapp_require_mention(&self) -> bool {
        whatsapp_require_mention(self.config_extra())
    }

    fn whatsapp_free_response_chats(&self) -> HashSet<String> {
        whatsapp_free_response_chats(self.config_extra())
    }

    fn live_dm_allow_from(&self) -> HashSet<String> {
        live_dm_allow_from(self.dm_allowlist_source(), self.allow_from())
    }

    /// Strict DM authorization -- pairing does not imply access.
    fn is_dm_allowed(&self, sender_id: &str) -> bool {
        match self.dm_policy() {
            "disabled" => false,
            "allowlist" => matches_whatsapp_allowlist(sender_id, &self.live_dm_allow_from()),
            "open" => open_dm_opted_in(),
            _ => false,
        }
    }

    /// Whether a DM may reach the gateway intake (pairing handshake path).
    fn is_dm_intake_allowed(&self, sender_id: &str) -> bool {
        let principal = sender_id.trim();
        if principal.is_empty() {
            return false;
        }
        match self.dm_policy() {
            "disabled" => false,
            "allowlist" => matches_whatsapp_allowlist(principal, &self.live_dm_allow_from()),
            "pairing" => true,
            "open" => open_dm_opted_in(),
            _ => false,
        }
    }

    /// Whether a group chat should be processed.
    fn is_group_allowed(&self, chat_id: &str) -> bool {
        match self.group_policy() {
            "disabled" => false,
            "allowlist" => matches_whatsapp_allowlist(chat_id, self.group_allow_from()),
            "pairing" => false,
            "open" => true,
            _ => false,
        }
    }

    fn compile_mention_patterns(&self) -> Vec<Regex> {
        compile_mention_patterns(self.config_extra(), self.adapter_name())
    }

    fn message_matches_mention_patterns(&self, data: &Value) -> bool {
        message_matches_mention_patterns(data, self.mention_patterns())
    }

    /// Port of `_should_process_message`.
    fn should_process_message(&self, data: &Value) -> bool {
        let chat_id_raw = dget_str_or_empty(data, "chatId");
        // WhatsApp uses pseudo-chats for Status updates (Stories) and
        // Channel/Newsletter broadcasts. These are not real conversations and
        // the agent should never reply to them, even in self-chat mode where the
        // bridge may surface them as "fromMe" events.
        if is_broadcast_chat(&chat_id_raw) {
            return false;
        }
        let is_group = dget(data, "isGroup").map(py_truthy).unwrap_or(false);
        if is_group {
            if !self.is_group_allowed(&chat_id_raw) {
                return false;
            }
        } else {
            let sender_id = match dget(data, "senderId").filter(|v| py_truthy(v)) {
                Some(v) => py_str(v),
                None => match dget(data, "from").filter(|v| py_truthy(v)) {
                    Some(v) => py_str(v),
                    None => String::new(),
                },
            };
            if !self.is_dm_intake_allowed(&sender_id) {
                return false;
            }
            // DMs that pass the policy gate are always processed.
            return true;
        }
        // Group messages: check mention / free-response settings.
        let chat_id = dget_str_or_empty(data, "chatId");
        if self.whatsapp_free_response_chats().contains(&chat_id) {
            return true;
        }
        if !self.whatsapp_require_mention() {
            return true;
        }
        let body = dget_str_or_empty(data, "body");
        if body.trim().starts_with('/') {
            return true;
        }
        if message_is_reply_to_bot(data) {
            return true;
        }
        if message_mentions_bot(data) {
            return true;
        }
        self.message_matches_mention_patterns(data)
    }

    fn format_message(&self, content: &str) -> String {
        format_message(content)
    }
}

// ---------------------------------------------------------------------------
// shared bridge directory resolution for CLI and adapter
// ---------------------------------------------------------------------------

/// The install tree root. Python uses `Path(__file__).resolve().parents[2]`,
/// which from `gateway/platforms/whatsapp_common.py` is the checkout root. Rust
/// has no runtime source path, so this walks up from the compile-time
/// `CARGO_MANIFEST_DIR` (`<root>/rust/crates/hermes-gateway`), the same approach
/// `code_skew.rs` takes. On a deployed binary the path may not exist, which just
/// means the writability probe fails and the HERMES_HOME mirror kicks in.
pub fn install_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

/// Resolve the WhatsApp bridge directory, mirroring to HERMES_HOME if needed.
/// Port of `resolve_whatsapp_bridge_dir`.
///
/// When the install tree is read-only (e.g. Docker `/opt/hermes`), the bridge
/// source is mirrored to a writable HERMES_HOME location and that path is
/// returned, so `npm install` works in Docker environments.
pub fn resolve_whatsapp_bridge_dir() -> PathBuf {
    resolve_whatsapp_bridge_dir_from(&install_root(), &crate::config_file::hermes_home())
}

/// [`resolve_whatsapp_bridge_dir`] with both roots injected, so the resolution
/// order is testable without an actual read-only filesystem.
///
/// Order, faithful to Python:
///  1. `<install_root>/scripts/whatsapp-bridge` if it is writable (probed with a
///     `.write_test` touch + unlink, exactly as Python does).
///  2. `<hermes_home>/scripts/whatsapp-bridge` if it already exists.
///  3. Otherwise mirror (1) into (2) and return (2); on any failure fall back to
///     returning (1).
pub fn resolve_whatsapp_bridge_dir_from(install_root: &Path, hermes_home: &Path) -> PathBuf {
    // Default location in the install tree (may be read-only).
    let install_bridge = install_root.join("scripts").join("whatsapp-bridge");
    let hermes_home_bridge = hermes_home.join("scripts").join("whatsapp-bridge");

    // Check if the install dir is writable. `Path.touch()` creates the file (or
    // just bumps mtime) and raises OSError when the parent is missing or
    // read-only; `unlink()` then removes it.
    let test_file = install_bridge.join(".write_test");
    let install_writable = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&test_file)
        .and_then(|_| std::fs::remove_file(&test_file))
        .is_ok();

    if install_writable {
        return install_bridge;
    }

    // Install dir is read-only: mirror to HERMES_HOME if needed.
    if hermes_home_bridge.exists() {
        return hermes_home_bridge;
    }

    // Mirror the bridge source to HERMES_HOME. `shutil.copytree(...,
    // dirs_exist_ok=False)` on a destination that does not exist, wrapped in a
    // bare `except` that falls back to the install path.
    let mirrored = hermes_home_bridge
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| copy_tree(&install_bridge, &hermes_home_bridge));
    match mirrored {
        Ok(()) => hermes_home_bridge,
        Err(_) => install_bridge,
    }
}

/// `shutil.copytree(src, dst, dirs_exist_ok=False)`: fails if `dst` already
/// exists, follows symlinks (copies the target's contents).
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", dst.display()),
        ));
    }
    std::fs::create_dir(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // metadata() follows symlinks, matching copytree's symlinks=False.
        if std::fs::metadata(&from)?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret_scope::GLOBAL_TEST_LOCK;
    use serde_json::json;

    // Golden values in this module were produced by running the Python module
    // directly:
    //   python3 -c "import sys; sys.path.insert(0,'.'); \
    //       from gateway.platforms.whatsapp_common import ..."

    fn tmpdir(sub: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_test_wa_common_{}_{}_{}",
            sub,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        p
    }

    // ---- a minimal adapter, standing in for the class that mixes this in ----

    struct FakeAdapter {
        extra: Map<String, Value>,
        name: String,
        dm_policy: String,
        allow_from: HashSet<String>,
        group_policy: String,
        group_allow_from: HashSet<String>,
        mention_patterns: Vec<Regex>,
        reply_prefix: Option<String>,
        dm_allowlist_source: Option<String>,
    }

    impl Default for FakeAdapter {
        fn default() -> Self {
            FakeAdapter {
                extra: Map::new(),
                name: "whatsapp".to_string(),
                dm_policy: "allowlist".to_string(),
                allow_from: HashSet::new(),
                group_policy: "disabled".to_string(),
                group_allow_from: HashSet::new(),
                mention_patterns: Vec::new(),
                reply_prefix: None,
                dm_allowlist_source: None,
            }
        }
    }

    impl WhatsAppBehavior for FakeAdapter {
        fn config_extra(&self) -> &Map<String, Value> {
            &self.extra
        }
        fn adapter_name(&self) -> &str {
            &self.name
        }
        fn dm_policy(&self) -> &str {
            &self.dm_policy
        }
        fn allow_from(&self) -> &HashSet<String> {
            &self.allow_from
        }
        fn group_policy(&self) -> &str {
            &self.group_policy
        }
        fn group_allow_from(&self) -> &HashSet<String> {
            &self.group_allow_from
        }
        fn mention_patterns(&self) -> &[Regex] {
            &self.mention_patterns
        }
        fn reply_prefix(&self) -> Option<&str> {
            self.reply_prefix.as_deref()
        }
        fn dm_allowlist_source(&self) -> Option<&str> {
            self.dm_allowlist_source.as_deref()
        }
    }

    // ---- _get_wsecret ----

    #[test]
    fn get_wsecret_scoped_and_unscoped() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();

        // Multiplex off, no scope: a plain environment read, default applied.
        crate::secret_scope::set_multiplex_active(false);
        std::env::set_var("WHATSAPP_TEST_SECRET", "envval");
        assert_eq!(
            get_wsecret("WHATSAPP_TEST_SECRET", None).as_deref(),
            Some("envval")
        );
        std::env::remove_var("WHATSAPP_TEST_SECRET");
        assert_eq!(get_wsecret("WHATSAPP_TEST_SECRET", None), None);
        assert_eq!(
            get_wsecret("WHATSAPP_TEST_SECRET", Some("d")).as_deref(),
            Some("d")
        );

        // Multiplex on with no scope: get_secret fails closed, and _get_wsecret
        // catches that and reads os.environ instead. This is the whole point of
        // the helper: the DEFAULT profile sends unscoped and must not crash.
        crate::secret_scope::set_multiplex_active(true);
        assert!(crate::secret_scope::get_secret("WHATSAPP_TEST_SECRET", None).is_err());
        std::env::set_var("WHATSAPP_TEST_SECRET", "own-profile-value");
        assert_eq!(
            get_wsecret("WHATSAPP_TEST_SECRET", None).as_deref(),
            Some("own-profile-value")
        );
        // Env miss under the fallback still lands on the default.
        std::env::remove_var("WHATSAPP_TEST_SECRET");
        assert_eq!(
            get_wsecret("WHATSAPP_TEST_SECRET", Some("d")).as_deref(),
            Some("d")
        );
        crate::secret_scope::set_multiplex_active(false);
    }

    #[test]
    fn get_wsecret_scope_is_authoritative_under_multiplexing() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let mut scope = std::collections::HashMap::new();
        scope.insert("WHATSAPP_MODE".to_string(), "group".to_string());

        crate::secret_scope::set_multiplex_active(true);
        std::env::set_var("WHATSAPP_OTHER_KEY", "other-profile-leak");
        rt.block_on(crate::secret_scope::with_secret_scope(Some(scope), async {
            assert_eq!(get_wsecret("WHATSAPP_MODE", None).as_deref(), Some("group"));
            // A scoped miss returns the default, never the environment.
            assert_eq!(get_wsecret("WHATSAPP_OTHER_KEY", None), None);
            assert_eq!(
                get_wsecret("WHATSAPP_OTHER_KEY", Some("d")).as_deref(),
                Some("d")
            );
        }));
        std::env::remove_var("WHATSAPP_OTHER_KEY");
        crate::secret_scope::set_multiplex_active(false);
    }

    // ---- _sanitize_outbound_text ----

    #[test]
    fn sanitize_outbound_text_golden() {
        assert_eq!(sanitize_outbound_text(""), "");
        assert_eq!(sanitize_outbound_text("a\u{200b}b"), "ab");
        assert_eq!(
            sanitize_outbound_text("x\u{a0}y\u{2009}z\u{3000}w"),
            "x y z w"
        );
        assert_eq!(sanitize_outbound_text("\u{2060}\u{202f} lead"), "  lead");
        // U+2063 and U+FEFF are stripped; U+1680 and U+180E become spaces.
        assert_eq!(
            sanitize_outbound_text("a\u{2063}\u{feff}b\u{1680}c\u{180e}d"),
            "ab c d"
        );
    }

    // ---- format_message ----

    #[test]
    fn format_message_golden() {
        // Every expectation below is the literal Python output.
        let cases: &[(&str, &str)] = &[
            ("", ""),
            (
                "**bold** and *italic* and ~~strike~~",
                "*bold* and _italic_ and ~strike~",
            ),
            (
                "# Header\n## **Sub**\n### ***Deep***",
                "*Header*\n*Sub*\n*Deep*",
            ),
            ("[link](https://x.com/a)", "link (https://x.com/a)"),
            (
                "```py\ncode **not bold**\n```\nafter **bold**",
                "```py\ncode **not bold**\n```\nafter *bold*",
            ),
            ("inline `a*b*c` and *it*", "inline `a*b*c` and _it_"),
            ("* bullet\n* another", "* bullet\n* another"),
            (
                "__underscore bold__ and _italic_",
                "*underscore bold* and _italic_",
            ),
            (
                "a \u{2060}\u{202f} b \u{200b}\u{feff}\u{a0}c\u{3000}d",
                "a   b  c d",
            ),
            (
                "text with $dollar and \\1 backref",
                "text with $dollar and \\1 backref",
            ),
            ("# **Title**", "*Title*"),
            ("#   spaced header   ", "*spaced header*"),
            ("**a**b**c**", "*a*b*c*"),
            ("*a* *b*", "_a_ _b_"),
            ("5 * 3 * 2", "5 * 3 * 2"),
            ("~~a~~ ~b~", "~a~ ~b~"),
            (
                "```\nfence1\n```mid```\nfence2\n```",
                "```\nfence1\n```mid```\nfence2\n```",
            ),
            ("[a](b) [c](d)", "a (b) c (d)"),
            ("## __Bold Head__", "*Bold Head*"),
        ];
        for (input, expected) in cases {
            assert_eq!(format_message(input), *expected, "input {input:?}");
        }
    }

    // ---- JID helpers ----

    #[test]
    fn normalize_whatsapp_id_golden() {
        assert_eq!(normalize_whatsapp_id(None), "");
        assert_eq!(normalize_whatsapp_id(Some("")), "");
        assert_eq!(normalize_whatsapp_id(Some("   ")), "");
        assert_eq!(
            normalize_whatsapp_id(Some("user:device@s.whatsapp.net")),
            "user@device@s.whatsapp.net"
        );
        assert_eq!(normalize_whatsapp_id(Some("1234@lid")), "1234@lid");
        // Only the ":" + "@" combination triggers the rewrite.
        assert_eq!(normalize_whatsapp_id(Some("a:b")), "a:b");
        assert_eq!(normalize_whatsapp_id(Some("a@b")), "a@b");
        assert_eq!(normalize_whatsapp_id(Some("  x:y@z  ")), "x@y@z");
        // Only the FIRST colon is rewritten.
        assert_eq!(normalize_whatsapp_id(Some("a:b:c@d")), "a@b:c@d");
        // Payload-value form: falsy values short-circuit, others go via str().
        assert_eq!(normalize_whatsapp_id_value(&Value::Null), "");
        assert_eq!(normalize_whatsapp_id_value(&json!(0)), "");
        assert_eq!(normalize_whatsapp_id_value(&json!(1234)), "1234");
    }

    #[test]
    fn is_broadcast_chat_golden() {
        assert!(!is_broadcast_chat(""));
        assert!(is_broadcast_chat("status@broadcast"));
        assert!(is_broadcast_chat(" STATUS@Broadcast "));
        assert!(is_broadcast_chat("123@newsletter"));
        assert!(!is_broadcast_chat("x@g.us"));
        assert!(is_broadcast_chat("a@BROADCAST"));
    }

    // ---- allow-list coercion ----

    fn sorted(set: HashSet<String>) -> Vec<String> {
        let mut v: Vec<String> = set.into_iter().collect();
        v.sort();
        v
    }

    #[test]
    fn coerce_allow_list_golden() {
        assert!(coerce_allow_list(None).is_empty());
        assert!(coerce_allow_list(Some(&Value::Null)).is_empty());
        assert!(coerce_allow_list(Some(&json!(""))).is_empty());
        assert_eq!(
            sorted(coerce_allow_list(Some(&json!("a, b ,,c")))),
            vec!["a", "b", "c"]
        );
        assert_eq!(
            sorted(coerce_allow_list(Some(&json!(["x", " y ", "", 3])))),
            vec!["3", "x", "y"]
        );
        assert_eq!(
            sorted(coerce_allow_list(Some(&json!("single")))),
            vec!["single"]
        );
        assert_eq!(sorted(coerce_allow_list(Some(&json!(42)))), vec!["42"]);
    }

    #[test]
    fn live_dm_allow_from_precedence() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        let snapshot: HashSet<String> = ["111".to_string(), "222".to_string()].into();

        // No source, or an explicit "config" source: keep the in-memory snapshot.
        assert_eq!(
            sorted(live_dm_allow_from(None, &snapshot)),
            vec!["111", "222"]
        );
        assert_eq!(
            sorted(live_dm_allow_from(Some("config"), &snapshot)),
            vec!["111", "222"]
        );

        // Env-seeded: re-read the key live, including an empty value.
        std::env::set_var("WA_LIVE_ALLOW_TEST", "333, 444");
        assert_eq!(
            sorted(live_dm_allow_from(Some("WA_LIVE_ALLOW_TEST"), &snapshot)),
            vec!["333", "444"]
        );
        std::env::set_var("WA_LIVE_ALLOW_TEST", "");
        assert!(live_dm_allow_from(Some("WA_LIVE_ALLOW_TEST"), &snapshot).is_empty());

        // Key removed entirely: empty, NOT the stale snapshot.
        std::env::remove_var("WA_LIVE_ALLOW_TEST");
        assert!(live_dm_allow_from(Some("WA_LIVE_ALLOW_TEST"), &snapshot).is_empty());
    }

    // ---- config-driven behavior ----

    #[test]
    fn effective_reply_prefix_and_chunk_limit_golden() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        crate::secret_scope::set_multiplex_active(false);
        std::env::remove_var("WHATSAPP_MODE");
        std::env::remove_var("WHATSAPP_REPLY_PREFIX");

        const DEFAULT: &str = "⚕ *Hermes Agent*\n────────────\n";
        // Python: len(DEFAULT_REPLY_PREFIX) == 30 (code points), limit 4066.
        assert_eq!(DEFAULT.chars().count(), 30);

        let mut a = FakeAdapter::default();
        assert_eq!(a.effective_reply_prefix(), DEFAULT);
        assert_eq!(a.outgoing_chunk_limit(), 4066);

        // An explicit prefix wins, with "\n" unescaped.
        a.reply_prefix = Some("Hi\\nThere".to_string());
        assert_eq!(a.effective_reply_prefix(), "Hi\nThere");
        assert_eq!(a.outgoing_chunk_limit(), 4088);

        // An explicit EMPTY prefix still wins (Python checks `is not None`).
        a.reply_prefix = Some(String::new());
        assert_eq!(a.effective_reply_prefix(), "");
        assert_eq!(a.outgoing_chunk_limit(), 4096);

        // Falls through to the env carrier when unset.
        a.reply_prefix = None;
        std::env::set_var("WHATSAPP_REPLY_PREFIX", "E\\nP");
        assert_eq!(a.effective_reply_prefix(), "E\nP");

        // Any non-self-chat mode suppresses the prefix entirely.
        std::env::set_var("WHATSAPP_MODE", "group");
        assert_eq!(a.effective_reply_prefix(), "");
        assert_eq!(a.outgoing_chunk_limit(), 4096);

        // An EMPTY mode is falsy in Python, so `or "self-chat"` restores it.
        std::env::set_var("WHATSAPP_MODE", "");
        assert_eq!(a.effective_reply_prefix(), "E\nP");

        std::env::remove_var("WHATSAPP_MODE");
        std::env::remove_var("WHATSAPP_REPLY_PREFIX");

        // The 1024 floor absorbs an absurdly long prefix.
        assert_eq!(outgoing_chunk_limit(4096, &"x".repeat(9000)), 1024);
        assert_eq!(outgoing_chunk_limit(4096, ""), 4096);
    }

    #[test]
    fn whatsapp_require_mention_config_and_env() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        crate::secret_scope::set_multiplex_active(false);
        std::env::remove_var("WHATSAPP_REQUIRE_MENTION");

        // Unset env: default "false".
        assert!(!whatsapp_require_mention(&Map::new()));
        std::env::set_var("WHATSAPP_REQUIRE_MENTION", "YES");
        assert!(whatsapp_require_mention(&Map::new()));
        std::env::set_var("WHATSAPP_REQUIRE_MENTION", "nope");
        assert!(!whatsapp_require_mention(&Map::new()));

        // Config wins over env, in both directions.
        let mut extra = Map::new();
        extra.insert("require_mention".into(), json!("On"));
        assert!(whatsapp_require_mention(&extra));
        extra.insert("require_mention".into(), json!(true));
        assert!(whatsapp_require_mention(&extra));
        extra.insert("require_mention".into(), json!(0));
        assert!(!whatsapp_require_mention(&extra));
        extra.insert("require_mention".into(), json!("maybe"));
        assert!(!whatsapp_require_mention(&extra));
        // A present-but-null config slot behaves like an absent one.
        std::env::set_var("WHATSAPP_REQUIRE_MENTION", "true");
        extra.insert("require_mention".into(), Value::Null);
        assert!(whatsapp_require_mention(&extra));

        std::env::remove_var("WHATSAPP_REQUIRE_MENTION");
    }

    #[test]
    fn whatsapp_free_response_chats_config_and_env() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        crate::secret_scope::set_multiplex_active(false);
        std::env::remove_var("WHATSAPP_FREE_RESPONSE_CHATS");

        assert!(whatsapp_free_response_chats(&Map::new()).is_empty());
        std::env::set_var("WHATSAPP_FREE_RESPONSE_CHATS", " a@g.us , b@g.us ,,");
        assert_eq!(
            sorted(whatsapp_free_response_chats(&Map::new())),
            vec!["a@g.us", "b@g.us"]
        );

        let mut extra = Map::new();
        extra.insert("free_response_chats".into(), json!(["x@g.us", " y@g.us "]));
        assert_eq!(
            sorted(whatsapp_free_response_chats(&extra)),
            vec!["x@g.us", "y@g.us"]
        );
        extra.insert("free_response_chats".into(), json!("z@g.us,w@g.us"));
        assert_eq!(
            sorted(whatsapp_free_response_chats(&extra)),
            vec!["w@g.us", "z@g.us"]
        );

        std::env::remove_var("WHATSAPP_FREE_RESPONSE_CHATS");
    }

    #[test]
    fn open_dm_opted_in_reads_both_keys() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        crate::secret_scope::set_multiplex_active(false);
        std::env::remove_var("GATEWAY_ALLOW_ALL_USERS");
        std::env::remove_var("WHATSAPP_ALLOW_ALL_USERS");
        assert!(!open_dm_opted_in());

        std::env::set_var("GATEWAY_ALLOW_ALL_USERS", "TRUE");
        assert!(open_dm_opted_in());
        std::env::set_var("GATEWAY_ALLOW_ALL_USERS", "on");
        // "on" is NOT in this set (unlike require_mention's set).
        assert!(!open_dm_opted_in());

        std::env::remove_var("GATEWAY_ALLOW_ALL_USERS");
        std::env::set_var("WHATSAPP_ALLOW_ALL_USERS", "1");
        assert!(open_dm_opted_in());
        std::env::remove_var("WHATSAPP_ALLOW_ALL_USERS");
    }

    // ---- mention patterns ----

    #[test]
    fn compile_mention_patterns_sources() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        crate::secret_scope::set_multiplex_active(false);
        std::env::remove_var("WHATSAPP_MENTION_PATTERNS");

        assert!(compile_mention_patterns(&Map::new(), "wa").is_empty());

        // JSON list from the env carrier.
        std::env::set_var("WHATSAPP_MENTION_PATTERNS", r#"["hermes", "^bot\\b"]"#);
        let compiled = compile_mention_patterns(&Map::new(), "wa");
        assert_eq!(compiled.len(), 2);
        // re.IGNORECASE is applied.
        assert!(compiled[0].is_match("Ask HERMES please").unwrap());

        // Non-JSON falls back to newline splitting.
        std::env::set_var("WHATSAPP_MENTION_PATTERNS", "alpha\n\nbeta\n");
        let compiled = compile_mention_patterns(&Map::new(), "wa");
        assert_eq!(compiled.len(), 2);

        // A JSON scalar that is neither a string nor a list yields nothing.
        std::env::set_var("WHATSAPP_MENTION_PATTERNS", "123");
        assert!(compile_mention_patterns(&Map::new(), "wa").is_empty());

        // Config wins; a bare string is wrapped into a one-element list.
        let mut extra = Map::new();
        extra.insert("mention_patterns".into(), json!("hey"));
        assert_eq!(compile_mention_patterns(&extra, "wa").len(), 1);
        // Blank and non-string entries are skipped; invalid regexes are dropped.
        extra.insert(
            "mention_patterns".into(),
            json!(["ok", "   ", 5, "([unclosed"]),
        );
        assert_eq!(compile_mention_patterns(&extra, "wa").len(), 1);
        // A dict is neither a string nor a list.
        extra.insert("mention_patterns".into(), json!({"a": 1}));
        assert!(compile_mention_patterns(&extra, "wa").is_empty());

        std::env::remove_var("WHATSAPP_MENTION_PATTERNS");
    }

    // ---- message inspection ----

    #[test]
    fn bot_ids_and_reply_detection_golden() {
        assert_eq!(
            sorted(bot_ids_from_message(
                &json!({"botIds": ["1234@lid", "", null]})
            )),
            vec!["1234@lid"]
        );
        assert!(bot_ids_from_message(&json!({})).is_empty());

        assert!(message_is_reply_to_bot(
            &json!({"botIds": ["1@l"], "quotedParticipant": "1@l"})
        ));
        assert!(!message_is_reply_to_bot(&json!({"botIds": ["1@l"]})));
        assert!(!message_is_reply_to_bot(
            &json!({"botIds": ["2@l"], "quotedParticipant": "1@l"})
        ));
    }

    #[test]
    fn message_mentions_bot_golden() {
        let cases: &[(Value, bool)] = &[
            (
                json!({"botIds": ["1234@lid"], "mentionedIds": ["1234@lid"]}),
                true,
            ),
            (
                json!({"botIds": ["1234@lid"], "body": "hey @1234 there"}),
                true,
            ),
            // The bare id alone counts, not just the @-form.
            (
                json!({"botIds": ["1234@lid"], "body": "hey 1234 there"}),
                true,
            ),
            (json!({"botIds": ["1234@lid"], "body": "nope"}), false),
            (json!({"botIds": [], "body": "@1234"}), false),
            // Case-insensitive on the bare id.
            (json!({"botIds": ["abc@lid"], "body": "ABC yes"}), true),
        ];
        for (data, expected) in cases {
            assert_eq!(message_mentions_bot(data), *expected, "data {data}");
        }
    }

    #[test]
    fn clean_bot_mention_text_golden() {
        let cases: &[(&str, Value, &str)] = &[
            (
                "@1234 hello",
                json!({"botIds": ["1234@s.whatsapp.net"]}),
                "hello",
            ),
            (
                "@1234, hi there",
                json!({"botIds": ["1234@lid"]}),
                "hi there",
            ),
            // Stripping everything leaves the ORIGINAL text, not "".
            ("@1234", json!({"botIds": ["1234@lid"]}), "@1234"),
            ("hello", json!({"botIds": []}), "hello"),
            ("", json!({"botIds": ["1@l"]}), ""),
            ("@1234-: spaced", json!({"botIds": ["1234@x"]}), "spaced"),
        ];
        for (text, data, expected) in cases {
            assert_eq!(
                clean_bot_mention_text(text, data),
                *expected,
                "text {text:?}"
            );
        }
    }

    #[test]
    fn message_matches_mention_patterns_golden() {
        let patterns = vec![RegexBuilder::new("hermes")
            .case_insensitive(true)
            .build()
            .unwrap()];
        assert!(message_matches_mention_patterns(
            &json!({"body": "hi HERMES"}),
            &patterns
        ));
        assert!(!message_matches_mention_patterns(
            &json!({"body": "hi there"}),
            &patterns
        ));
        // No patterns configured: always false, even for a matching body.
        assert!(!message_matches_mention_patterns(
            &json!({"body": "hermes"}),
            &[]
        ));
    }

    // ---- gating / should_process_message ----

    #[test]
    fn dm_and_group_policy_gating() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        crate::secret_scope::set_multiplex_active(false);
        std::env::remove_var("GATEWAY_ALLOW_ALL_USERS");
        std::env::remove_var("WHATSAPP_ALLOW_ALL_USERS");

        let mut a = FakeAdapter {
            dm_policy: "disabled".into(),
            ..FakeAdapter::default()
        };

        assert!(!a.is_dm_allowed("x"));
        assert!(!a.is_dm_intake_allowed("x"));

        // "pairing" lets a DM reach intake but is NOT strict authorization.
        a.dm_policy = "pairing".into();
        assert!(a.is_dm_intake_allowed("x"));
        assert!(!a.is_dm_allowed("x"));
        // A blank principal never reaches intake.
        assert!(!a.is_dm_intake_allowed("   "));

        a.dm_policy = "open".into();
        assert!(!a.is_dm_allowed("x"));
        std::env::set_var("GATEWAY_ALLOW_ALL_USERS", "yes");
        assert!(a.is_dm_allowed("x"));
        assert!(a.is_dm_intake_allowed("x"));
        std::env::remove_var("GATEWAY_ALLOW_ALL_USERS");

        // An unknown policy string is closed, not open.
        a.dm_policy = "bogus".into();
        assert!(!a.is_dm_allowed("x"));
        assert!(!a.is_dm_intake_allowed("x"));

        // Group: "pairing" is closed, unlike the DM path.
        a.group_policy = "pairing".into();
        assert!(!a.is_group_allowed("g@g.us"));
        a.group_policy = "open".into();
        assert!(a.is_group_allowed("g@g.us"));
        a.group_policy = "disabled".into();
        assert!(!a.is_group_allowed("g@g.us"));
        a.group_policy = "bogus".into();
        assert!(!a.is_group_allowed("g@g.us"));
    }

    #[test]
    fn matches_whatsapp_allowlist_forms() {
        let dir = tmpdir("allowlist");
        std::fs::create_dir_all(&dir).unwrap();

        let phone = "15551234567";
        let lid = "999999999999999";
        std::fs::write(
            dir.join(format!("lid-mapping-{lid}.json")),
            format!("\"{phone}@s.whatsapp.net\""),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("lid-mapping-{phone}_reverse.json")),
            format!("\"{lid}\""),
        )
        .unwrap();

        let empty: HashSet<String> = HashSet::new();
        assert!(!matches_whatsapp_allowlist_in_dir("anything", &empty, &dir));

        // Exact raw match, no alias resolution needed (a full group JID).
        let group: HashSet<String> = ["123-456@g.us".to_string()].into();
        assert!(matches_whatsapp_allowlist_in_dir(
            "123-456@g.us",
            &group,
            &dir
        ));
        assert!(!matches_whatsapp_allowlist_in_dir(
            "other@g.us",
            &group,
            &dir
        ));

        // Wildcard.
        let star: HashSet<String> = ["*".to_string()].into();
        assert!(matches_whatsapp_allowlist_in_dir(
            "15551234567@lid",
            &star,
            &dir
        ));

        // A phone allowlist entry matches an inbound LID, and vice versa.
        let by_phone: HashSet<String> = [format!("+{phone}")].into();
        assert!(matches_whatsapp_allowlist_in_dir(
            &format!("{lid}@lid"),
            &by_phone,
            &dir
        ));
        let by_lid: HashSet<String> = [lid.to_string()].into();
        assert!(matches_whatsapp_allowlist_in_dir(
            &format!("{phone}@s.whatsapp.net"),
            &by_lid,
            &dir
        ));

        // An unrelated number does not match.
        assert!(!matches_whatsapp_allowlist_in_dir(
            "18005550000",
            &by_lid,
            &dir
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_process_message_flow() {
        let _g = GLOBAL_TEST_LOCK.lock().unwrap();
        crate::secret_scope::set_multiplex_active(false);
        std::env::remove_var("WHATSAPP_REQUIRE_MENTION");
        std::env::remove_var("WHATSAPP_FREE_RESPONSE_CHATS");
        std::env::remove_var("GATEWAY_ALLOW_ALL_USERS");
        std::env::remove_var("WHATSAPP_ALLOW_ALL_USERS");

        let mut a = FakeAdapter {
            dm_policy: "pairing".into(),
            group_policy: "open".into(),
            ..FakeAdapter::default()
        };

        // Broadcast pseudo-chats are dropped before anything else.
        assert!(!a.should_process_message(&json!({"chatId": "status@broadcast"})));
        assert!(!a.should_process_message(&json!({"chatId": "9@newsletter", "isGroup": true})));

        // DM that passes the policy gate is always processed.
        assert!(a.should_process_message(&json!({"chatId": "u@s.whatsapp.net", "senderId": "u"})));
        // "from" is the fallback carrier for the sender.
        assert!(a.should_process_message(&json!({"chatId": "u@s.whatsapp.net", "from": "u"})));
        // No sender at all -> blank principal -> refused.
        assert!(!a.should_process_message(&json!({"chatId": "u@s.whatsapp.net"})));

        // Group, require_mention off: everything passes.
        let group = json!({"chatId": "g@g.us", "isGroup": true, "body": "hi"});
        assert!(a.should_process_message(&group));

        // Group, require_mention on: needs a slash command, a reply, a mention
        // or a pattern hit.
        std::env::set_var("WHATSAPP_REQUIRE_MENTION", "true");
        assert!(!a.should_process_message(&group));
        assert!(a.should_process_message(
            &json!({"chatId": "g@g.us", "isGroup": true, "body": "  /help"})
        ));
        assert!(a.should_process_message(&json!({
            "chatId": "g@g.us", "isGroup": true, "body": "yo",
            "botIds": ["77@lid"], "quotedParticipant": "77@lid"
        })));
        assert!(a.should_process_message(&json!({
            "chatId": "g@g.us", "isGroup": true, "body": "yo @77",
            "botIds": ["77@lid"]
        })));

        // A free-response chat bypasses the mention requirement.
        std::env::set_var("WHATSAPP_FREE_RESPONSE_CHATS", "g@g.us");
        assert!(a.should_process_message(&group));
        std::env::remove_var("WHATSAPP_FREE_RESPONSE_CHATS");

        // A configured mention pattern is the last resort.
        a.mention_patterns = vec![RegexBuilder::new("hermes")
            .case_insensitive(true)
            .build()
            .unwrap()];
        assert!(a.should_process_message(
            &json!({"chatId": "g@g.us", "isGroup": true, "body": "ping Hermes"})
        ));

        // A disallowed group is refused before any of that.
        a.group_policy = "disabled".into();
        assert!(!a.should_process_message(&group));

        std::env::remove_var("WHATSAPP_REQUIRE_MENTION");
    }

    // ---- bridge directory resolution ----

    #[test]
    fn resolve_whatsapp_bridge_dir_golden() {
        // Golden from Python in this checkout:
        //   /home/eins0fx/development/hermes-agent-port/scripts/whatsapp-bridge
        // i.e. <repo root>/scripts/whatsapp-bridge, because the install tree is
        // writable here.
        let expected = install_root().join("scripts").join("whatsapp-bridge");
        assert_eq!(resolve_whatsapp_bridge_dir(), expected);
    }

    #[test]
    fn resolve_bridge_dir_prefers_writable_install_tree() {
        let root = tmpdir("writable_install");
        let bridge = root.join("scripts").join("whatsapp-bridge");
        std::fs::create_dir_all(&bridge).unwrap();
        std::fs::write(bridge.join("bridge.js"), b"// x").unwrap();
        let home = tmpdir("writable_home");

        assert_eq!(resolve_whatsapp_bridge_dir_from(&root, &home), bridge);
        // The probe file must not survive.
        assert!(!bridge.join(".write_test").exists());
        // Nothing was mirrored.
        assert!(!home.join("scripts").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_bridge_dir_uses_existing_home_copy() {
        // A missing install tree makes the touch probe fail, which is the same
        // "not writable" branch a read-only /opt/hermes takes.
        let root = tmpdir("missing_install");
        let home = tmpdir("existing_home");
        let home_bridge = home.join("scripts").join("whatsapp-bridge");
        std::fs::create_dir_all(&home_bridge).unwrap();

        assert_eq!(resolve_whatsapp_bridge_dir_from(&root, &home), home_bridge);

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_bridge_dir_falls_back_when_mirror_impossible() {
        // No install tree to copy from and no existing home copy: the copytree
        // raises, and Python's bare except returns the install path.
        let root = tmpdir("no_install");
        let home = tmpdir("no_home");
        let expected = root.join("scripts").join("whatsapp-bridge");
        assert_eq!(resolve_whatsapp_bridge_dir_from(&root, &home), expected);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn copy_tree_mirrors_recursively() {
        let src = tmpdir("copy_src");
        std::fs::create_dir_all(src.join("node")).unwrap();
        std::fs::write(src.join("bridge.js"), b"top").unwrap();
        std::fs::write(src.join("node").join("dep.js"), b"nested").unwrap();
        let dst = tmpdir("copy_dst");

        copy_tree(&src, &dst).unwrap();
        assert_eq!(std::fs::read(dst.join("bridge.js")).unwrap(), b"top");
        assert_eq!(
            std::fs::read(dst.join("node").join("dep.js")).unwrap(),
            b"nested"
        );
        // dirs_exist_ok=False: a second copy onto the same destination fails.
        assert!(copy_tree(&src, &dst).is_err());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }
}
