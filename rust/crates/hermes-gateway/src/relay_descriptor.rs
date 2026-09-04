//! Port of gateway/relay/descriptor.py.
//!
// Public API is ahead of its callers (wired later).
#![allow(dead_code)]
//! CapabilityDescriptor, the relay handshake payload (EXPERIMENTAL). The
//! connector hands a [`CapabilityDescriptor`] to the gateway's RelayAdapter at
//! handshake time; it names the platform being fronted and the capabilities to
//! advertise to the stream consumer (char limit, draft-streaming, edit/threading
//! support, markdown dialect, length unit, context/continuable/block-formatting
//! flags, and the op-discovery set). One gateway adapter then serves Discord,
//! Telegram, Matrix, Signal and so on without per-platform branching.
//!
//! The Python module has no internal imports (only `json` and `dataclasses`),
//! so this is a self-contained port. Two faithfulness notes:
//!
//! - The Python `@dataclass(frozen=True)` does no type checking, so its
//!   `from_json` lets a wrong-typed field flow straight into the frozen
//!   instance (e.g. `contract_version="X"` or `supports_edit=1`). A typed Rust
//!   struct cannot store those, so [`CapabilityDescriptor::from_json`] returns a
//!   [`DescriptorError`] on a wrong JSON type for a required field, where Python
//!   would silently keep the odd value (it fails the same way downstream). The
//!   two documented normalizations (`max_message_length` and `supported_ops`)
//!   are reproduced exactly. A real handshake always carries correct types, so
//!   the happy path matches byte-for-byte.
//! - `max_message_length` is an `i64` here. Python keeps the *original* value
//!   when `int(value) > 0` (so a float `2000.7` or string `"10"` survives), but
//!   maps anything with `int(value) <= 0` or a failed `int()` to 4096. For the
//!   real case (a JSON integer) the behaviors are identical (5 -> 5, 0 -> 4096,
//!   -3 -> 4096); for the degenerate float/string/bool cases this stores the
//!   truncated integer instead of the odd original.
//!
//! [`CapabilityDescriptor::from_platform_entry`] is duck-typed in Python against
//! `gateway.platform_registry.PlatformEntry` (plain `getattr` with defaults, no
//! import). The Rust `platform::PlatformEntry` port does not yet carry the fields
//! it reads, so it is ported here against the local [`PlatformEntryLike`] trait,
//! which mirrors exactly that `getattr`-with-defaults projection.

/// Bumped additively during the experimental phase; a breaking change requires
/// updating both repos in lockstep.
pub const CONTRACT_VERSION: i64 = 1;

/// The 🔌 default emoji (matches PlatformEntry's default).
pub const DEFAULT_EMOJI: &str = "\u{1f50c}";

/// The op set every connector supported before `supported_ops` existed. Used as
/// the assumed capability set when a legacy connector sends no list.
pub const LEGACY_OPS: [&str; 4] = ["send", "edit", "typing", "follow_up"];

/// Malformed handshake input. Mirrors the point where Python's `from_json` would
/// raise (a `TypeError` on missing/odd fields, an `AttributeError` on a non-object
/// body). The message is diagnostic, not a stable wire contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorError(pub String);

impl std::fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DescriptorError {}

fn err(message: impl Into<String>) -> DescriptorError {
    DescriptorError(message.into())
}

/// Immutable capability descriptor negotiated at relay handshake.
///
/// The Python dataclass is `frozen=True` so a descriptor cannot be mutated after
/// handshake. Here the fields are public for construction and read but the type
/// carries no interior mutability, matching that fixed-profile intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    pub contract_version: i64,
    pub platform: String,
    pub label: String,
    pub max_message_length: i64,
    pub supports_draft_streaming: bool,
    pub supports_edit: bool,
    pub supports_threads: bool,
    pub markdown_dialect: String,
    /// "chars" | "utf16".
    pub len_unit: String,
    pub emoji: String,
    pub platform_hint: String,
    pub pii_safe: bool,
    pub supports_context: bool,
    pub supports_inchannel_continuable: bool,
    pub supports_block_formatting: bool,
    /// Op-level capability discovery. Empty = the connector predates the field;
    /// callers MUST treat that as the legacy op set (see [`supports_op`]).
    ///
    /// [`supports_op`]: CapabilityDescriptor::supports_op
    pub supported_ops: Vec<String>,
}

impl CapabilityDescriptor {
    /// Construct with the nine required fields; the optional fields take the same
    /// defaults as the Python dataclass (🔌 emoji, empty hint, all flags false,
    /// empty op list). Mirrors the positional dataclass constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        contract_version: i64,
        platform: impl Into<String>,
        label: impl Into<String>,
        max_message_length: i64,
        supports_draft_streaming: bool,
        supports_edit: bool,
        supports_threads: bool,
        markdown_dialect: impl Into<String>,
        len_unit: impl Into<String>,
    ) -> CapabilityDescriptor {
        CapabilityDescriptor {
            contract_version,
            platform: platform.into(),
            label: label.into(),
            max_message_length,
            supports_draft_streaming,
            supports_edit,
            supports_threads,
            markdown_dialect: markdown_dialect.into(),
            len_unit: len_unit.into(),
            emoji: DEFAULT_EMOJI.to_string(),
            platform_hint: String::new(),
            pii_safe: false,
            supports_context: false,
            supports_inchannel_continuable: false,
            supports_block_formatting: false,
            supported_ops: Vec::new(),
        }
    }

    /// Whether the connector advertises the outbound op `op`.
    ///
    /// Fail-open for legacy connectors: an empty `supported_ops` means the
    /// connector predates op discovery, so assume [`LEGACY_OPS`]. A new op (e.g.
    /// `get_chat_info`) is therefore only true when explicitly advertised.
    pub fn supports_op(&self, op: &str) -> bool {
        if self.supported_ops.is_empty() {
            LEGACY_OPS.contains(&op)
        } else {
            self.supported_ops.iter().any(|o| o == op)
        }
    }

    /// Serialize to a compact, stable JSON string for the handshake frame.
    ///
    /// Byte-matches Python `json.dumps(asdict(self), sort_keys=True,
    /// ensure_ascii=False)`: keys sorted ascending, `", "` between members and
    /// `": "` after each key, and non-ASCII left literal (only `"`, `\`, the
    /// short escapes and control chars below 0x20 are escaped).
    pub fn to_json(&self) -> String {
        // Emit in fixed ascending key order (matches sort_keys=True for this
        // exact, ASCII, known key set).
        let mut out = String::new();
        out.push('{');

        push_int(&mut out, "contract_version", self.contract_version, true);
        push_str(&mut out, "emoji", &self.emoji, false);
        push_str(&mut out, "label", &self.label, false);
        push_str(&mut out, "len_unit", &self.len_unit, false);
        push_str(&mut out, "markdown_dialect", &self.markdown_dialect, false);
        push_int(
            &mut out,
            "max_message_length",
            self.max_message_length,
            false,
        );
        push_bool(&mut out, "pii_safe", self.pii_safe, false);
        push_str(&mut out, "platform", &self.platform, false);
        push_str(&mut out, "platform_hint", &self.platform_hint, false);
        push_str_array(&mut out, "supported_ops", &self.supported_ops, false);
        push_bool(
            &mut out,
            "supports_block_formatting",
            self.supports_block_formatting,
            false,
        );
        push_bool(&mut out, "supports_context", self.supports_context, false);
        push_bool(
            &mut out,
            "supports_draft_streaming",
            self.supports_draft_streaming,
            false,
        );
        push_bool(&mut out, "supports_edit", self.supports_edit, false);
        push_bool(
            &mut out,
            "supports_inchannel_continuable",
            self.supports_inchannel_continuable,
            false,
        );
        push_bool(&mut out, "supports_threads", self.supports_threads, false);

        out.push('}');
        out
    }

    /// Deserialize from a handshake JSON string.
    ///
    /// Unknown keys are ignored (forward-compat), missing optional keys fall back
    /// to defaults, and the two documented trust-boundary normalizations run:
    /// `max_message_length` maps a non-positive or non-integer value to 4096, and
    /// `supported_ops` degrades any non-list or non-string element to the legacy
    /// fallback (an empty list). See the module doc for the typed-language caveat
    /// on wrong-typed required fields.
    pub fn from_json(data: &str) -> Result<CapabilityDescriptor, DescriptorError> {
        let raw: serde_json::Value = serde_json::from_str(data)
            .map_err(|e| err(format!("descriptor JSON is invalid: {e}")))?;
        let object = match &raw {
            serde_json::Value::Object(m) => m,
            // Python does `raw.items()`, which raises AttributeError on a non-dict.
            _ => return Err(err("descriptor payload must be a JSON object")),
        };

        let contract_version = req_int(object, "contract_version")?;
        let platform = req_string(object, "platform")?;
        let label = req_string(object, "label")?;
        let max_message_length = normalize_max_message_length(object.get("max_message_length"))?;
        let supports_draft_streaming = req_bool(object, "supports_draft_streaming")?;
        let supports_edit = req_bool(object, "supports_edit")?;
        let supports_threads = req_bool(object, "supports_threads")?;
        let markdown_dialect = req_string(object, "markdown_dialect")?;
        let len_unit = req_string(object, "len_unit")?;

        let emoji = opt_string(object, "emoji", DEFAULT_EMOJI)?;
        let platform_hint = opt_string(object, "platform_hint", "")?;
        let pii_safe = opt_bool(object, "pii_safe", false)?;
        let supports_context = opt_bool(object, "supports_context", false)?;
        let supports_inchannel_continuable =
            opt_bool(object, "supports_inchannel_continuable", false)?;
        let supports_block_formatting = opt_bool(object, "supports_block_formatting", false)?;
        let supported_ops = normalize_supported_ops(object.get("supported_ops"));

        Ok(CapabilityDescriptor {
            contract_version,
            platform,
            label,
            max_message_length,
            supports_draft_streaming,
            supports_edit,
            supports_threads,
            markdown_dialect,
            len_unit,
            emoji,
            platform_hint,
            pii_safe,
            supports_context,
            supports_inchannel_continuable,
            supports_block_formatting,
            supported_ops,
        })
    }

    /// Project a [`PlatformEntryLike`] into a descriptor. The label, char limit,
    /// emoji, hint, PII flag and platform name come straight off the entry; the
    /// runtime capability bits the entry does not encode (length unit, draft/edit/
    /// thread/markdown behavior) are supplied by the caller, matching the Python
    /// keyword arguments with the same defaults.
    ///
    /// A `max_message_length` of 0 on the entry means "no limit"; it maps to the
    /// stream-consumer default of 4096 (Python `... or 4096`, which fires only on
    /// the falsy 0, so a negative would pass through unchanged here).
    pub fn from_platform_entry(
        entry: &impl PlatformEntryLike,
        len_unit: impl Into<String>,
        supports_draft_streaming: bool,
        supports_edit: bool,
        supports_threads: bool,
        markdown_dialect: impl Into<String>,
    ) -> CapabilityDescriptor {
        let raw_len = entry.max_message_length();
        let max_len = if raw_len == 0 { 4096 } else { raw_len };
        CapabilityDescriptor {
            contract_version: CONTRACT_VERSION,
            platform: entry.name().to_string(),
            label: entry.label().to_string(),
            max_message_length: max_len,
            supports_draft_streaming,
            supports_edit,
            supports_threads,
            markdown_dialect: markdown_dialect.into(),
            len_unit: len_unit.into(),
            emoji: entry.emoji(),
            platform_hint: entry.platform_hint(),
            pii_safe: entry.pii_safe(),
            supports_context: false,
            supports_inchannel_continuable: false,
            supports_block_formatting: false,
            supported_ops: Vec::new(),
        }
    }
}

/// The duck-typed projection source for [`CapabilityDescriptor::from_platform_entry`].
/// `name` and `label` are read directly in Python; the rest come through
/// `getattr(entry, ..., default)`, so they are provided methods carrying those
/// same defaults (0, 🔌, empty hint, not PII-safe). A concrete platform entry
/// overrides only the ones it actually has.
pub trait PlatformEntryLike {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn max_message_length(&self) -> i64 {
        0
    }
    fn emoji(&self) -> String {
        DEFAULT_EMOJI.to_string()
    }
    fn platform_hint(&self) -> String {
        String::new()
    }
    fn pii_safe(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// from_json field readers and normalizers
// ---------------------------------------------------------------------------

fn req_int(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<i64, DescriptorError> {
    match object.get(field) {
        None => Err(err(format!(
            "descriptor is missing required field: {field}"
        ))),
        // Reject bools (JSON bool is not a Python int here) and floats.
        Some(serde_json::Value::Number(n)) if !n.is_f64() => n
            .as_i64()
            .ok_or_else(|| err(format!("{field} must be an integer"))),
        Some(_) => Err(err(format!("{field} must be an integer"))),
    }
}

fn req_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<String, DescriptorError> {
    match object.get(field) {
        None => Err(err(format!(
            "descriptor is missing required field: {field}"
        ))),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(err(format!("{field} must be a string"))),
    }
}

fn req_bool(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<bool, DescriptorError> {
    match object.get(field) {
        None => Err(err(format!(
            "descriptor is missing required field: {field}"
        ))),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(_) => Err(err(format!("{field} must be a boolean"))),
    }
}

fn opt_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    default: &str,
) -> Result<String, DescriptorError> {
    match object.get(field) {
        None => Ok(default.to_string()),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(err(format!("{field} must be a string"))),
    }
}

fn opt_bool(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    default: bool,
) -> Result<bool, DescriptorError> {
    match object.get(field) {
        None => Ok(default),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(_) => Err(err(format!("{field} must be a boolean"))),
    }
}

/// Port of the `max_message_length` normalization: Python computes `int(value)`
/// and maps `<= 0` or a failed conversion to 4096; the value survives otherwise.
/// A JSON integer maps identically; a float truncates toward zero (Python
/// `int()`); a string parses as a strict integer (Python `int(str)`); a bool
/// counts as 1/0; anything else fails the conversion and yields 4096. A missing
/// key means the field was never present, so the caller must still get a value:
/// Python leaves it to the dataclass, which has no default for this required
/// field and raises, so a missing key is an error here.
fn normalize_max_message_length(value: Option<&serde_json::Value>) -> Result<i64, DescriptorError> {
    let value = match value {
        Some(v) => v,
        None => {
            return Err(err(
                "descriptor is missing required field: max_message_length",
            ))
        }
    };
    let coerced: Option<i64> = match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else {
                // Float: Python int() truncates toward zero.
                n.as_f64().map(|f| f.trunc() as i64)
            }
        }
        serde_json::Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        serde_json::Value::String(s) => parse_python_int(s),
        _ => None,
    };
    match coerced {
        Some(i) if i > 0 => Ok(i),
        _ => Ok(4096),
    }
}

/// Parse a string the way Python `int(str)` does: optional surrounding
/// whitespace, an optional sign, then only ASCII digits (so "5.0" and "0x10"
/// fail). Returns None on anything Python would reject.
fn parse_python_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (sign, digits) = match t.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().map(|n| sign * n)
}

/// Port of the `supported_ops` normalization: a JSON list keeps its non-empty
/// string elements in order; any non-list (or missing) value degrades to the
/// empty legacy fallback.
fn normalize_supported_ops(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// to_json serialization helpers (ensure_ascii=False, default separators)
// ---------------------------------------------------------------------------

fn push_prefix(out: &mut String, key: &str, first: bool) {
    if !first {
        out.push_str(", ");
    }
    write_json_string(out, key);
    out.push_str(": ");
}

fn push_str(out: &mut String, key: &str, value: &str, first: bool) {
    push_prefix(out, key, first);
    write_json_string(out, value);
}

fn push_int(out: &mut String, key: &str, value: i64, first: bool) {
    push_prefix(out, key, first);
    out.push_str(&value.to_string());
}

fn push_bool(out: &mut String, key: &str, value: bool, first: bool) {
    push_prefix(out, key, first);
    out.push_str(if value { "true" } else { "false" });
}

fn push_str_array(out: &mut String, key: &str, values: &[String], first: bool) {
    push_prefix(out, key, first);
    out.push('[');
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_json_string(out, v);
    }
    out.push(']');
}

/// Append `s` as a JSON string with `ensure_ascii=False` escaping, matching
/// CPython's encoder: the short escapes for the control characters that have
/// them, `\u00xx` for the rest below 0x20, and every other character (0x20 and
/// up, including 0x7f and all non-ASCII) written literally.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeEntry {
        name: &'static str,
        label: &'static str,
        max_message_length: i64,
        emoji: Option<String>,
        platform_hint: Option<String>,
        pii_safe: Option<bool>,
    }

    impl PlatformEntryLike for FakeEntry {
        fn name(&self) -> &str {
            self.name
        }
        fn label(&self) -> &str {
            self.label
        }
        fn max_message_length(&self) -> i64 {
            self.max_message_length
        }
        fn emoji(&self) -> String {
            self.emoji
                .clone()
                .unwrap_or_else(|| DEFAULT_EMOJI.to_string())
        }
        fn platform_hint(&self) -> String {
            self.platform_hint.clone().unwrap_or_default()
        }
        fn pii_safe(&self) -> bool {
            self.pii_safe.unwrap_or(false)
        }
    }

    // Golden strings produced by the real Python module:
    //   CapabilityDescriptor(...).to_json()

    #[test]
    fn to_json_defaults_match_python_byte_for_byte() {
        let d = CapabilityDescriptor::new(
            1, "discord", "Discord", 2000, true, true, true, "discord", "chars",
        );
        assert_eq!(
            d.to_json(),
            r#"{"contract_version": 1, "emoji": "🔌", "label": "Discord", "len_unit": "chars", "markdown_dialect": "discord", "max_message_length": 2000, "pii_safe": false, "platform": "discord", "platform_hint": "", "supported_ops": [], "supports_block_formatting": false, "supports_context": false, "supports_draft_streaming": true, "supports_edit": true, "supports_inchannel_continuable": false, "supports_threads": true}"#
        );
    }

    #[test]
    fn to_json_all_fields_set_match_python_byte_for_byte() {
        let d = CapabilityDescriptor {
            contract_version: 1,
            platform: "p".into(),
            label: "L".into(),
            max_message_length: 100,
            supports_draft_streaming: false,
            supports_edit: false,
            supports_threads: false,
            markdown_dialect: "plain".into(),
            len_unit: "utf16".into(),
            emoji: "x".into(),
            platform_hint: "hint".into(),
            pii_safe: true,
            supports_context: true,
            supports_inchannel_continuable: true,
            supports_block_formatting: true,
            supported_ops: vec!["send".into(), "edit".into(), "typing".into()],
        };
        assert_eq!(
            d.to_json(),
            r#"{"contract_version": 1, "emoji": "x", "label": "L", "len_unit": "utf16", "markdown_dialect": "plain", "max_message_length": 100, "pii_safe": true, "platform": "p", "platform_hint": "hint", "supported_ops": ["send", "edit", "typing"], "supports_block_formatting": true, "supports_context": true, "supports_draft_streaming": false, "supports_edit": false, "supports_inchannel_continuable": true, "supports_threads": false}"#
        );
    }

    #[test]
    fn to_json_escaping_ensure_ascii_false() {
        // Non-ASCII stays literal; control chars and quote/backslash are escaped.
        let mut d =
            CapabilityDescriptor::new(1, "p", "L", 10, false, false, false, "plain", "chars");
        d.platform_hint = "a\"b\\c\nd\te\u{01}f\u{e9}".into();
        // Python json.dumps(..., ensure_ascii=False): quote/backslash and the
        // short escapes are escaped, 0x01 becomes \u0001, U+00E9 stays literal.
        let expected = "\"platform_hint\": \"a\\\"b\\\\c\\nd\\te\\u0001f\u{e9}\"";
        assert!(d.to_json().contains(expected), "got: {}", d.to_json());
    }

    #[test]
    fn from_json_normalizes_and_filters() {
        let j = r#"{
            "contract_version": 1, "platform": "p", "label": "L",
            "max_message_length": 0,
            "supports_draft_streaming": true, "supports_edit": true,
            "supports_threads": false,
            "markdown_dialect": "plain", "len_unit": "chars",
            "unknown_field": "ignored",
            "supported_ops": ["a", "", "b"]
        }"#;
        let d = CapabilityDescriptor::from_json(j).unwrap();
        // 0 -> 4096 (documented no-limit normalization).
        assert_eq!(d.max_message_length, 4096);
        // Empty strings dropped, order preserved, unknown key ignored.
        assert_eq!(d.supported_ops, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn from_json_max_message_length_cases_match_python() {
        // Values and expected outputs taken from the real Python from_json.
        let cases: &[(&str, i64)] = &[
            ("5", 5),
            ("0", 4096),
            ("-3", 4096),
            ("2000.7", 2000), // Python keeps 2000.7; typed i64 stores int(2000.7)
            ("\"10\"", 10),   // Python keeps "10"; typed i64 stores 10
            ("\"5.0\"", 4096),
            ("true", 1),
            ("false", 4096),
            ("null", 4096),
            ("[1]", 4096),
        ];
        for (mml, expected) in cases {
            let j = format!(
                r#"{{"contract_version":1,"platform":"p","label":"L","max_message_length":{mml},"supports_draft_streaming":false,"supports_edit":false,"supports_threads":false,"markdown_dialect":"x","len_unit":"chars"}}"#
            );
            let d = CapabilityDescriptor::from_json(&j).unwrap();
            assert_eq!(d.max_message_length, *expected, "mml input {mml}");
        }
    }

    #[test]
    fn from_json_supported_ops_non_list_degrades() {
        let j = r#"{"contract_version":1,"platform":"p","label":"L","max_message_length":10,"supports_draft_streaming":false,"supports_edit":false,"supports_threads":false,"markdown_dialect":"x","len_unit":"chars","supported_ops":"notalist"}"#;
        let d = CapabilityDescriptor::from_json(j).unwrap();
        assert!(d.supported_ops.is_empty());
    }

    #[test]
    fn from_json_missing_required_field_errors() {
        let j = r#"{"platform":"p"}"#;
        assert!(CapabilityDescriptor::from_json(j).is_err());
    }

    #[test]
    fn from_json_non_object_errors() {
        assert!(CapabilityDescriptor::from_json("[1,2]").is_err());
    }

    #[test]
    fn from_json_optional_defaults() {
        let j = r#"{"contract_version":1,"platform":"p","label":"L","max_message_length":10,"supports_draft_streaming":false,"supports_edit":false,"supports_threads":false,"markdown_dialect":"x","len_unit":"chars"}"#;
        let d = CapabilityDescriptor::from_json(j).unwrap();
        assert_eq!(d.emoji, DEFAULT_EMOJI);
        assert_eq!(d.platform_hint, "");
        assert!(!d.pii_safe);
        assert!(!d.supports_context);
        assert!(!d.supports_inchannel_continuable);
        assert!(!d.supports_block_formatting);
        assert!(d.supported_ops.is_empty());
    }

    #[test]
    fn to_json_from_json_round_trip() {
        let d = CapabilityDescriptor::new(
            1,
            "telegram",
            "Telegram",
            4096,
            true,
            true,
            false,
            "markdown_v2",
            "utf16",
        );
        let back = CapabilityDescriptor::from_json(&d.to_json()).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn supports_op_legacy_fallback() {
        // Empty supported_ops -> the four legacy ops, and nothing else.
        let d = CapabilityDescriptor::new(1, "p", "L", 10, false, false, false, "x", "chars");
        assert!(d.supports_op("send"));
        assert!(d.supports_op("edit"));
        assert!(d.supports_op("typing"));
        assert!(d.supports_op("follow_up"));
        assert!(!d.supports_op("get_chat_info"));
    }

    #[test]
    fn supports_op_explicit_set() {
        let mut d = CapabilityDescriptor::new(1, "p", "L", 10, false, false, false, "x", "chars");
        d.supported_ops = vec!["send".into(), "get_chat_info".into()];
        assert!(d.supports_op("send"));
        assert!(d.supports_op("get_chat_info"));
        // "edit" is a legacy op but NOT advertised, so false once a set exists.
        assert!(!d.supports_op("edit"));
    }

    #[test]
    fn from_platform_entry_projects_and_defaults() {
        let entry = FakeEntry {
            name: "discord",
            label: "Discord",
            max_message_length: 2000,
            emoji: Some("D".into()),
            platform_hint: Some("hint".into()),
            pii_safe: Some(true),
        };
        let d = CapabilityDescriptor::from_platform_entry(
            &entry, "chars", true, true, false, "discord",
        );
        assert_eq!(d.contract_version, CONTRACT_VERSION);
        assert_eq!(d.platform, "discord");
        assert_eq!(d.label, "Discord");
        assert_eq!(d.max_message_length, 2000);
        assert_eq!(d.emoji, "D");
        assert_eq!(d.platform_hint, "hint");
        assert!(d.pii_safe);
        assert!(d.supports_draft_streaming);
        assert!(d.supports_edit);
        assert!(!d.supports_threads);
        assert_eq!(d.len_unit, "chars");
        assert_eq!(d.markdown_dialect, "discord");
        // Fields the entry never carries default to false/empty.
        assert!(!d.supports_context);
        assert!(d.supported_ops.is_empty());
    }

    #[test]
    fn from_platform_entry_zero_length_maps_to_default() {
        // getattr default 0 -> `... or 4096`. Uses the trait defaults for the
        // optional attributes.
        let entry = FakeEntry {
            name: "signal",
            label: "Signal",
            max_message_length: 0,
            emoji: None,
            platform_hint: None,
            pii_safe: None,
        };
        let d = CapabilityDescriptor::from_platform_entry(
            &entry, "chars", false, false, false, "plain",
        );
        assert_eq!(d.max_message_length, 4096);
        assert_eq!(d.emoji, DEFAULT_EMOJI);
        assert_eq!(d.platform_hint, "");
        assert!(!d.pii_safe);
    }
}
