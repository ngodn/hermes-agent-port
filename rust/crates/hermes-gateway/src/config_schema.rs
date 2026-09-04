//! Port of the Platform enum and pure coercion helpers from gateway/config.py.
//!
// Public API is ahead of its callers while the gateway config pipeline is ported.
#![allow(dead_code)]
//!
//! This is the pure, self-contained tier of `gateway/config.py`: the `Platform`
//! enum, the port-binding classification, and the small family of value
//! coercion / normalization helpers used while loading gateway config. The big
//! config dataclasses (HomeChannel, GatewayConfig, ...) and `load_gateway_config`
//! live elsewhere and are not ported here.
//!
//! Python inputs typed `Any` become `serde_json::Value`, matching the other
//! ported gateway modules (see `display_config.rs`). Coercion follows Python
//! semantics exactly: `.strip().lower()`, the shared truthy/falsy string sets,
//! and the "catch (TypeError, ValueError) -> default" fallbacks.

use serde_json::{Map, Value};

/// Shared truthy string set from `utils.TRUTHY_STRINGS`.
/// `frozenset({"1", "true", "yes", "on"})`.
const TRUTHY_STRINGS: &[&str] = &["1", "true", "yes", "on"];

fn is_truthy_string(s: &str) -> bool {
    let lowered = s.trim().to_lowercase();
    TRUTHY_STRINGS.contains(&lowered.as_str())
}

/// Python `bool(value)` for the non-None, non-bool, non-str cases that reach the
/// tail of `is_truthy_value`: numbers are falsy at zero, containers are falsy
/// when empty.
fn python_bool(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Port of `utils.is_truthy_value`.
///
/// ```text
/// None            -> default
/// bool            -> the bool itself (default ignored)
/// str             -> stripped/lowered in TRUTHY_STRINGS
/// anything else   -> bool(value)
/// ```
fn is_truthy_value(value: &Value, default: bool) -> bool {
    match value {
        Value::Null => default,
        Value::Bool(b) => *b,
        Value::String(s) => is_truthy_string(s),
        other => python_bool(other),
    }
}

/// Port of `_coerce_bool`. Preserves a caller-provided default for None and for
/// unrecognized strings. Recognized truthy/falsy strings win outright; any other
/// non-string value falls through to `is_truthy_value`.
pub fn coerce_bool(value: &Value, default: bool) -> bool {
    match value {
        Value::Null => default,
        Value::String(s) => {
            let lowered = s.trim().to_lowercase();
            match lowered.as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => default,
            }
        }
        other => is_truthy_value(other, default),
    }
}

// --- Multiplex profile allowlist / env override -----------------------------

/// Recognized truthy tokens for the GATEWAY_MULTIPLEX_PROFILES override.
const MULTIPLEX_TRUTHY_STRINGS: &[&str] = &["1", "true", "yes", "on"];
/// Recognized falsy tokens for the GATEWAY_MULTIPLEX_PROFILES override.
const MULTIPLEX_FALSY_STRINGS: &[&str] = &["0", "false", "no", "off"];

/// Reserved profile names from `hermes_cli.profiles._RESERVED_NAMES`.
const RESERVED_PROFILE_NAMES: &[&str] = &["default", "hermes", "root", "sudo", "test", "tmp"];

/// Port of `hermes_cli.profiles.normalize_profile_name` for the subset reached
/// here (inputs are already strings). Empty after strip is an error; the
/// `default` alias is matched case-insensitively; everything else lowercases.
fn normalize_profile_name(name: &str) -> Result<String, ()> {
    let stripped = name.trim();
    if stripped.is_empty() {
        return Err(());
    }
    if stripped.to_lowercase() == "default" {
        return Ok("default".to_string());
    }
    Ok(stripped.to_lowercase())
}

/// True when `name` matches the profile id regex `^[a-z0-9][a-z0-9_-]{0,63}$`.
fn matches_profile_id_re(name: &str) -> bool {
    let len = name.chars().count();
    if len == 0 || len > 64 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Port of `hermes_cli.profiles.validate_profile_name`: strict lowercase match,
/// with `default` as a special pass-through and the reserved set rejected.
fn validate_profile_name(name: &str) -> Result<(), ()> {
    if name == "default" {
        return Ok(());
    }
    if !matches_profile_id_re(name) {
        return Err(());
    }
    if RESERVED_PROFILE_NAMES.contains(&name) {
        return Err(());
    }
    Ok(())
}

/// Port of `_normalize_multiplex_profile_allowlist`.
///
/// `None` preserves serve-all behavior. A non-list fails safe to an empty list
/// (default profile only). Malformed list entries are skipped. The `default`
/// alias and duplicates are dropped.
pub fn normalize_multiplex_profile_allowlist(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Null => None,
        Value::Array(entries) => {
            let mut normalized: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for entry in entries {
                let raw = match entry {
                    Value::String(s) => s,
                    // Non-string entry: skipped with a warning in Python.
                    _ => continue,
                };
                let name = match normalize_profile_name(raw) {
                    Ok(n) => n,
                    Err(()) => continue,
                };
                if validate_profile_name(&name).is_err() {
                    continue;
                }
                if name == "default" || seen.contains(&name) {
                    continue;
                }
                seen.insert(name.clone());
                normalized.push(name);
            }
            Some(normalized)
        }
        // Any non-null, non-list value: serve only the default profile.
        _ => Some(Vec::new()),
    }
}

/// Port of `_env_multiplex_profiles_override`.
///
/// Returns `Some(true)`/`Some(false)` when GATEWAY_MULTIPLEX_PROFILES is set to a
/// recognized token, or `None` when unset, blank, or unrecognized (blank stays
/// `None`, deliberately, so an empty Fly secret cannot shadow config.yaml).
pub fn env_multiplex_profiles_override() -> Option<bool> {
    let raw = std::env::var("GATEWAY_MULTIPLEX_PROFILES").ok()?;
    let token = raw.trim().to_lowercase();
    if token.is_empty() {
        return None;
    }
    if MULTIPLEX_TRUTHY_STRINGS.contains(&token.as_str()) {
        return Some(true);
    }
    if MULTIPLEX_FALSY_STRINGS.contains(&token.as_str()) {
        return Some(false);
    }
    None
}

// --- Value coercers ---------------------------------------------------------

/// Port of `_normalize_transport_token`.
///
/// Handles the YAML 1.1 boolean quirk: bare `on`/`off` parse to Python
/// True/False, which must map to `"auto"`/`"off"` rather than `"true"`/`"false"`.
/// Anything else is lower-cased, defaulting to `"auto"` when empty.
pub fn normalize_transport_token(value: &Value) -> String {
    match value {
        Value::Null => "auto".to_string(),
        Value::Bool(b) => {
            if *b {
                "auto".to_string()
            } else {
                "off".to_string()
            }
        }
        other => {
            let s = python_str(other);
            let lowered = s.trim().to_lowercase();
            if lowered.is_empty() {
                "auto".to_string()
            } else {
                lowered
            }
        }
    }
}

/// Minimal `str(value)` for the value shapes these helpers actually receive.
/// Strings pass through; numbers render like Python; other JSON shapes fall back
/// to their JSON text, which is close enough for the mode/token strings here.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// Port of `_coerce_float`. None -> default; otherwise Python `float(value)`,
/// falling back to default on malformed input.
pub fn coerce_float(value: &Value, default: f64) -> f64 {
    match value {
        Value::Null => default,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => n.as_f64().unwrap_or(default),
        Value::String(s) => {
            // Python float() strips surrounding whitespace and accepts
            // inf / nan spellings, which Rust's f64 parser also handles.
            match s.trim().parse::<f64>() {
                Ok(f) => f,
                Err(_) => default,
            }
        }
        // list / dict: Python float() raises TypeError -> default.
        _ => default,
    }
}

/// Port of `_coerce_int`. None -> default; otherwise Python `int(value)`,
/// falling back to default on TypeError / ValueError / OverflowError.
///
/// Careful bits: `int(4.9)` truncates toward zero, `int(float("inf"))`
/// overflows (so a non-finite value degrades to default), and `int(str)`
/// rejects decimals like `"1.5"` and bases like `"0x10"`.
pub fn coerce_int(value: &Value, default: i64) -> i64 {
    match value {
        Value::Null => default,
        Value::Bool(b) => {
            if *b {
                1
            } else {
                0
            }
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(u) = n.as_u64() {
                // Out of i64 range in JSON; Python's int is unbounded, this is a
                // best-effort narrowing for the realistic config ranges.
                u as i64
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    // int() truncates toward zero.
                    f.trunc() as i64
                } else {
                    default
                }
            } else {
                default
            }
        }
        Value::String(s) => {
            // Python int(str) trims whitespace and rejects any non-integer text.
            match s.trim().parse::<i64>() {
                Ok(i) => i,
                Err(_) => default,
            }
        }
        // list / dict: int() raises TypeError -> default.
        _ => default,
    }
}

/// Port of `_coerce_optional_positive_int`.
///
/// None/0/negative disable the setting (return None). Booleans are invalid.
/// Non-integer floats are invalid. Strings are parsed base 10. Anything <= 0
/// after coercion becomes None.
pub fn coerce_optional_positive_int(value: &Value) -> Option<i64> {
    let parsed: i64 = match value {
        Value::Null => return None,
        // isinstance(value, bool) is checked before int in Python: invalid.
        Value::Bool(_) => return None,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(u) = n.as_u64() {
                u as i64
            } else if let Some(f) = n.as_f64() {
                // Python: a float must be integer-valued, else ValueError.
                if f.is_finite() && f.fract() == 0.0 {
                    f as i64
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        Value::String(s) => match s.trim().parse::<i64>() {
            Ok(i) => i,
            Err(_) => return None,
        },
        // list / dict: int() raises TypeError -> None.
        _ => return None,
    };
    if parsed <= 0 {
        None
    } else {
        Some(parsed)
    }
}

/// Upper bound for the systemd watchdog interval (`_SYSTEMD_WATCHDOG_MAX_SECONDS`).
pub const SYSTEMD_WATCHDOG_MAX_SECONDS: i64 = 2_147_483_647;

/// Port of `coerce_systemd_watchdog_seconds`.
///
/// Returns a bounded positive interval, or 0 when disabled or invalid. Only
/// integers and all-ASCII-decimal strings are accepted; floats, signs, hex, and
/// blanks all degrade to 0. A value must land in 1..=MAX (inclusive) to enable.
pub fn coerce_systemd_watchdog_seconds(value: &Value) -> i64 {
    let parsed: i64 = match value {
        Value::Null => return 0,
        // isinstance(value, bool) is rejected before the int branch.
        Value::Bool(_) => return 0,
        Value::Number(n) => {
            // Python: only isinstance(value, int) is accepted here; a float
            // (or a value outside i64) falls through to the invalid branch.
            match n.as_i64() {
                Some(i) => i,
                None => return 0,
            }
        }
        Value::String(s) => {
            let raw = s.trim();
            // raw.isascii() and raw.isdecimal(): non-empty, all ASCII digits.
            if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
                return 0;
            }
            match raw.parse::<i64>() {
                Ok(i) => i,
                Err(_) => return 0,
            }
        }
        _ => return 0,
    };
    if parsed == 0 {
        return 0;
    }
    if !(0 < parsed && parsed <= SYSTEMD_WATCHDOG_MAX_SECONDS) {
        return 0;
    }
    parsed
}

/// Port of `_coerce_dict`: return the object as a map, else an empty map.
pub fn coerce_dict(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    }
}

/// Port of `_normalize_unauthorized_dm_behavior`. Accepts `pair`/`ignore`
/// (stripped/lowered), else the default.
pub fn normalize_unauthorized_dm_behavior(value: &Value, default: &str) -> String {
    if let Value::String(s) = value {
        let normalized = s.trim().to_lowercase();
        if normalized == "pair" || normalized == "ignore" {
            return normalized;
        }
    }
    default.to_string()
}

/// Port of `_normalize_notice_delivery`. Accepts `public`/`private`
/// (stripped/lowered), else the default.
pub fn normalize_notice_delivery(value: &Value, default: &str) -> String {
    if let Value::String(s) = value {
        let normalized = s.trim().to_lowercase();
        if normalized == "public" || normalized == "private" {
            return normalized;
        }
    }
    default.to_string()
}

/// Port of `_ensure_platform_extra_dict`.
///
/// Get-or-create `platforms_data[name]` and its nested `extra` map, coercing
/// either to `{}` when a non-dict value is found. Python returns
/// `(plat_data, extra)` for in-place mutation; here the outer map is mutated in
/// place and a mutable reference to the inner `extra` map is returned (the outer
/// entry stays reachable as `platforms_data[name]`).
pub fn ensure_platform_extra_dict<'a>(
    platforms_data: &'a mut Map<String, Value>,
    name: &str,
) -> &'a mut Map<String, Value> {
    let plat = platforms_data
        .entry(name.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !plat.is_object() {
        *plat = Value::Object(Map::new());
    }
    let plat_obj = plat.as_object_mut().expect("coerced to object above");
    let extra = plat_obj
        .entry("extra".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !extra.is_object() {
        *extra = Value::Object(Map::new());
    }
    extra.as_object_mut().expect("coerced to object above")
}

// --- Env readers ------------------------------------------------------------
//
// The Python `_getenv` prefers the active profile secret scope when one is
// installed; that scope is not ported here, so these mirror the unscoped
// `os.environ` fallback path used by single-profile callers.

/// Port of `_getenv` (unscoped path): the env var, else the default.
pub fn getenv(name: &str, default: Option<&str>) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => Some(v),
        Err(_) => default.map(|d| d.to_string()),
    }
}

/// Port of `_getenv_str`: the env var as a string, else the default.
pub fn getenv_str(name: &str, default: &str) -> String {
    getenv(name, Some(default)).unwrap_or_else(|| default.to_string())
}

/// Port of `_getenv_int`: parse the env var base 10, else the default.
pub fn getenv_int(name: &str, default: i64) -> i64 {
    match getenv(name, None) {
        None => default,
        Some(raw) => match raw.trim().parse::<i64>() {
            Ok(i) => i,
            Err(_) => default,
        },
    }
}

// --- Platform enum ----------------------------------------------------------

/// Supported messaging platforms (built-in members of `gateway.config.Platform`).
///
/// Python's enum also mints dynamic pseudo-members for bundled/registered plugin
/// platforms via `_missing_`; that filesystem/registry-driven path is not ported
/// here, so `from_value` recognizes the built-in values only (matching the
/// `_BUILTIN_PLATFORM_VALUES` snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Local,
    Telegram,
    Discord,
    Whatsapp,
    WhatsappCloud,
    Slack,
    Signal,
    Mattermost,
    Matrix,
    Homeassistant,
    Email,
    Sms,
    Dingtalk,
    ApiServer,
    Webhook,
    MsgraphWebhook,
    Feishu,
    Wecom,
    WecomCallback,
    Weixin,
    Bluebubbles,
    Qqbot,
    Yuanbao,
    Relay,
}

impl Platform {
    /// All built-in members, in declaration order.
    pub const ALL: &'static [Platform] = &[
        Platform::Local,
        Platform::Telegram,
        Platform::Discord,
        Platform::Whatsapp,
        Platform::WhatsappCloud,
        Platform::Slack,
        Platform::Signal,
        Platform::Mattermost,
        Platform::Matrix,
        Platform::Homeassistant,
        Platform::Email,
        Platform::Sms,
        Platform::Dingtalk,
        Platform::ApiServer,
        Platform::Webhook,
        Platform::MsgraphWebhook,
        Platform::Feishu,
        Platform::Wecom,
        Platform::WecomCallback,
        Platform::Weixin,
        Platform::Bluebubbles,
        Platform::Qqbot,
        Platform::Yuanbao,
        Platform::Relay,
    ];

    /// The `.value` string of the enum member.
    pub fn value(&self) -> &'static str {
        match self {
            Platform::Local => "local",
            Platform::Telegram => "telegram",
            Platform::Discord => "discord",
            Platform::Whatsapp => "whatsapp",
            Platform::WhatsappCloud => "whatsapp_cloud",
            Platform::Slack => "slack",
            Platform::Signal => "signal",
            Platform::Mattermost => "mattermost",
            Platform::Matrix => "matrix",
            Platform::Homeassistant => "homeassistant",
            Platform::Email => "email",
            Platform::Sms => "sms",
            Platform::Dingtalk => "dingtalk",
            Platform::ApiServer => "api_server",
            Platform::Webhook => "webhook",
            Platform::MsgraphWebhook => "msgraph_webhook",
            Platform::Feishu => "feishu",
            Platform::Wecom => "wecom",
            Platform::WecomCallback => "wecom_callback",
            Platform::Weixin => "weixin",
            Platform::Bluebubbles => "bluebubbles",
            Platform::Qqbot => "qqbot",
            Platform::Yuanbao => "yuanbao",
            Platform::Relay => "relay",
        }
    }

    /// Look up a built-in platform by its `.value` string, mirroring
    /// `Platform(value)` for the built-in tier. Unknown values return `None`
    /// (Python would either mint a plugin pseudo-member or raise ValueError).
    pub fn from_value(value: &str) -> Option<Platform> {
        Platform::ALL.iter().copied().find(|p| p.value() == value)
    }
}

/// Snapshot of built-in platform `.value` strings (`_BUILTIN_PLATFORM_VALUES`).
pub fn is_builtin_platform_value(value: &str) -> bool {
    Platform::from_value(value).is_some()
}

// --- Port-binding classification --------------------------------------------

/// Platforms that bind a host TCP port (`PORT_BINDING_PLATFORM_VALUES`).
/// Includes plugin platforms (`line`, `teams`) that are not enum members.
pub const PORT_BINDING_PLATFORM_VALUES: &[&str] = &[
    "webhook",
    "api_server",
    "msgraph_webhook",
    "feishu",
    "wecom_callback",
    "bluebubbles",
    "sms",
    "whatsapp_cloud",
    "line",
    "teams",
];

/// True when `value` is in `PORT_BINDING_PLATFORM_VALUES`.
pub fn is_port_binding_platform_value(value: &str) -> bool {
    PORT_BINDING_PLATFORM_VALUES.contains(&value)
}

/// Conditional port binding by connection mode (`PORT_BINDING_CONDITIONAL_MODES`).
/// Feishu binds a port only in `webhook` mode; its default websocket mode does
/// not. Returns the mode that actually binds, if the platform is conditional.
fn port_binding_conditional_mode(platform_value: &str) -> Option<&'static str> {
    match platform_value {
        "feishu" => Some("webhook"),
        _ => None,
    }
}

/// Port of `platform_binds_port`.
///
/// Returns true when `platform_value` actually binds a port for the given
/// `extra` config. Mode-conditional platforms (Feishu) bind only in their
/// listener mode; every other member of `PORT_BINDING_PLATFORM_VALUES` always
/// binds. `extra` is the platform's `extra` config object, if any.
pub fn platform_binds_port(platform_value: &str, extra: Option<&Value>) -> bool {
    if !is_port_binding_platform_value(platform_value) {
        return false;
    }
    if let Some(expected_mode) = port_binding_conditional_mode(platform_value) {
        // str((extra or {}).get("connection_mode", "websocket")).strip().lower()
        let mode_value = extra
            .and_then(|e| e.get("connection_mode"))
            .map(python_str)
            .unwrap_or_else(|| "websocket".to_string());
        let actual = mode_value.trim().to_lowercase();
        return actual == expected_mode;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- coerce_bool --------------------------------------------------------

    #[test]
    fn coerce_bool_none_uses_default() {
        assert!(coerce_bool(&Value::Null, true));
        assert!(!coerce_bool(&Value::Null, false));
    }

    #[test]
    fn coerce_bool_strings() {
        assert!(coerce_bool(&json!("  YES "), false));
        assert!(!coerce_bool(&json!("off"), true));
        // Unrecognized string falls back to the provided default.
        assert!(coerce_bool(&json!("maybe"), true));
        assert!(!coerce_bool(&json!("maybe"), false));
    }

    #[test]
    fn coerce_bool_non_string() {
        // Numbers / containers route through is_truthy_value (default ignored).
        assert!(coerce_bool(&json!(1), false));
        assert!(!coerce_bool(&json!(0), true));
        assert!(coerce_bool(&json!(5), false));
        assert!(!coerce_bool(&json!([]), true));
        assert!(coerce_bool(&json!([1]), false));
        // A real bool returns itself.
        assert!(coerce_bool(&json!(true), false));
        assert!(!coerce_bool(&json!(false), true));
    }

    // --- coerce_float -------------------------------------------------------

    #[test]
    fn coerce_float_cases() {
        assert_eq!(coerce_float(&json!("1.5"), 2.0), 1.5);
        assert_eq!(coerce_float(&json!("bad"), 2.0), 2.0);
        assert_eq!(coerce_float(&Value::Null, 9.0), 9.0);
        assert_eq!(coerce_float(&json!(true), 0.0), 1.0);
        assert_eq!(coerce_float(&json!(3), 0.0), 3.0);
        // list -> TypeError -> default
        assert_eq!(coerce_float(&json!([1]), 7.0), 7.0);
    }

    // --- coerce_int ---------------------------------------------------------

    #[test]
    fn coerce_int_cases() {
        assert_eq!(coerce_int(&json!("5x"), 3), 3);
        assert_eq!(coerce_int(&json!("7"), 3), 7);
        assert_eq!(coerce_int(&json!(" 12 "), 0), 12);
        // int(4.9) truncates toward zero.
        assert_eq!(coerce_int(&json!(4.9), 0), 4);
        assert_eq!(coerce_int(&json!(-4.9), 0), -4);
        assert_eq!(coerce_int(&Value::Null, 8), 8);
        // int(True) == 1
        assert_eq!(coerce_int(&json!(true), 0), 1);
        // Decimal / hex strings are rejected.
        assert_eq!(coerce_int(&json!("1.5"), 0), 0);
        assert_eq!(coerce_int(&json!("0x10"), 0), 0);
    }

    // --- coerce_optional_positive_int --------------------------------------

    #[test]
    fn coerce_optional_positive_int_cases() {
        assert_eq!(coerce_optional_positive_int(&Value::Null), None);
        // bool is invalid
        assert_eq!(coerce_optional_positive_int(&json!(true)), None);
        // 0 / negative disable
        assert_eq!(coerce_optional_positive_int(&json!(0)), None);
        assert_eq!(coerce_optional_positive_int(&json!(-3)), None);
        // stripped string
        assert_eq!(coerce_optional_positive_int(&json!("  5  ")), Some(5));
        // integer-valued float ok, fractional float invalid
        assert_eq!(coerce_optional_positive_int(&json!(4.0)), Some(4));
        assert_eq!(coerce_optional_positive_int(&json!(4.5)), None);
        // malformed string
        assert_eq!(coerce_optional_positive_int(&json!("bad")), None);
    }

    // --- coerce_systemd_watchdog_seconds -----------------------------------

    #[test]
    fn watchdog_valid_and_disabled() {
        assert_eq!(coerce_systemd_watchdog_seconds(&Value::Null), 0);
        assert_eq!(coerce_systemd_watchdog_seconds(&json!(true)), 0);
        assert_eq!(coerce_systemd_watchdog_seconds(&json!(0)), 0);
        assert_eq!(coerce_systemd_watchdog_seconds(&json!(5)), 5);
        assert_eq!(coerce_systemd_watchdog_seconds(&json!("10")), 10);
    }

    #[test]
    fn watchdog_invalid_strings_disable() {
        assert_eq!(coerce_systemd_watchdog_seconds(&json!("  ")), 0);
        // Signs / decimals / hex are not isdecimal.
        assert_eq!(coerce_systemd_watchdog_seconds(&json!("-4")), 0);
        assert_eq!(coerce_systemd_watchdog_seconds(&json!("1.5")), 0);
        assert_eq!(coerce_systemd_watchdog_seconds(&json!("0x10")), 0);
    }

    #[test]
    fn watchdog_clamp() {
        assert_eq!(coerce_systemd_watchdog_seconds(&json!(-4)), 0);
        assert_eq!(
            coerce_systemd_watchdog_seconds(&json!(SYSTEMD_WATCHDOG_MAX_SECONDS)),
            SYSTEMD_WATCHDOG_MAX_SECONDS
        );
        assert_eq!(
            coerce_systemd_watchdog_seconds(&json!(SYSTEMD_WATCHDOG_MAX_SECONDS + 1)),
            0
        );
        assert_eq!(SYSTEMD_WATCHDOG_MAX_SECONDS, 2_147_483_647);
    }

    // --- transport token ----------------------------------------------------

    #[test]
    fn transport_token_cases() {
        assert_eq!(normalize_transport_token(&Value::Null), "auto");
        assert_eq!(normalize_transport_token(&json!(true)), "auto");
        assert_eq!(normalize_transport_token(&json!(false)), "off");
        assert_eq!(normalize_transport_token(&json!(" WS ")), "ws");
        assert_eq!(normalize_transport_token(&json!("")), "auto");
        assert_eq!(normalize_transport_token(&json!("   ")), "auto");
    }

    // --- dm behavior / notice delivery -------------------------------------

    #[test]
    fn unauthorized_dm_behavior_cases() {
        assert_eq!(
            normalize_unauthorized_dm_behavior(&Value::Null, "pair"),
            "pair"
        );
        assert_eq!(
            normalize_unauthorized_dm_behavior(&json!(" IGNORE "), "pair"),
            "ignore"
        );
        assert_eq!(
            normalize_unauthorized_dm_behavior(&json!("x"), "pair"),
            "pair"
        );
        assert_eq!(
            normalize_unauthorized_dm_behavior(&json!("x"), "ignore"),
            "ignore"
        );
    }

    #[test]
    fn notice_delivery_cases() {
        assert_eq!(normalize_notice_delivery(&Value::Null, "public"), "public");
        assert_eq!(
            normalize_notice_delivery(&json!(" PRIVATE "), "public"),
            "private"
        );
        assert_eq!(normalize_notice_delivery(&json!("x"), "public"), "public");
    }

    // --- coerce_dict --------------------------------------------------------

    #[test]
    fn coerce_dict_cases() {
        let obj = json!({"a": 1});
        assert_eq!(coerce_dict(&obj), obj.as_object().unwrap().clone());
        assert!(coerce_dict(&json!([1, 2])).is_empty());
        assert!(coerce_dict(&Value::Null).is_empty());
    }

    // --- ensure_platform_extra_dict ----------------------------------------

    #[test]
    fn ensure_platform_extra_dict_creates_structure() {
        let mut platforms = Map::new();
        {
            let extra = ensure_platform_extra_dict(&mut platforms, "telegram");
            extra.insert("token".to_string(), json!("abc"));
        }
        assert_eq!(platforms["telegram"]["extra"]["token"], json!("abc"));
    }

    #[test]
    fn ensure_platform_extra_dict_coerces_non_dicts() {
        let mut platforms = Map::new();
        platforms.insert("telegram".to_string(), json!("not a dict"));
        {
            let extra = ensure_platform_extra_dict(&mut platforms, "telegram");
            assert!(extra.is_empty());
            extra.insert("k".to_string(), json!(1));
        }
        assert!(platforms["telegram"].is_object());
        assert_eq!(platforms["telegram"]["extra"]["k"], json!(1));

        // Non-dict extra is also coerced.
        let mut platforms2 = Map::new();
        platforms2.insert("slack".to_string(), json!({"extra": "bad"}));
        {
            let extra = ensure_platform_extra_dict(&mut platforms2, "slack");
            assert!(extra.is_empty());
        }
        assert!(platforms2["slack"]["extra"].is_object());
    }

    // --- multiplex allowlist ------------------------------------------------

    #[test]
    fn multiplex_allowlist_cases() {
        assert_eq!(normalize_multiplex_profile_allowlist(&Value::Null), None);
        // Non-list value -> empty list (default profile only).
        assert_eq!(
            normalize_multiplex_profile_allowlist(&json!("nope")),
            Some(vec![])
        );
        assert_eq!(
            normalize_multiplex_profile_allowlist(&json!([])),
            Some(vec![])
        );
        // Mixed: case-fold + dedupe + drop default/non-string/invalid/reserved.
        let input = json!(["Alpha", "beta", "beta", "default", " Gamma ", 5, "bad/name", "hermes"]);
        assert_eq!(
            normalize_multiplex_profile_allowlist(&input),
            Some(vec![
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string()
            ])
        );
    }

    // --- Platform enum ------------------------------------------------------

    #[test]
    fn platform_round_trip() {
        for p in Platform::ALL {
            assert_eq!(Platform::from_value(p.value()), Some(*p));
        }
        assert_eq!(Platform::from_value("signal"), Some(Platform::Signal));
        assert_eq!(Platform::from_value("local"), Some(Platform::Local));
        assert_eq!(
            Platform::from_value("whatsapp_cloud"),
            Some(Platform::WhatsappCloud)
        );
        assert_eq!(Platform::Signal.value(), "signal");
        assert_eq!(Platform::WhatsappCloud.value(), "whatsapp_cloud");
        assert_eq!(Platform::ApiServer.value(), "api_server");
    }

    #[test]
    fn platform_unknown_value() {
        assert_eq!(Platform::from_value("irc"), None);
        assert_eq!(Platform::from_value("SIGNAL"), None); // case-sensitive builtin lookup
        assert_eq!(Platform::from_value(""), None);
        assert!(is_builtin_platform_value("telegram"));
        assert!(!is_builtin_platform_value("line"));
    }

    #[test]
    fn platform_member_count() {
        assert_eq!(Platform::ALL.len(), 24);
    }

    // --- platform_binds_port -----------------------------------------------

    #[test]
    fn platform_binds_port_cases() {
        // Not a port-binding platform.
        assert!(!platform_binds_port("telegram", None));
        // Always-binding platforms.
        assert!(platform_binds_port("webhook", None));
        assert!(platform_binds_port("line", None));
        assert!(platform_binds_port("teams", None));
        // Feishu is conditional: websocket (default) does not bind.
        assert!(!platform_binds_port("feishu", None));
        // Feishu in webhook mode binds.
        let extra = json!({"connection_mode": "webhook"});
        assert!(platform_binds_port("feishu", Some(&extra)));
        // Feishu explicit websocket does not.
        let ws = json!({"connection_mode": "websocket"});
        assert!(!platform_binds_port("feishu", Some(&ws)));
    }

    // --- env readers --------------------------------------------------------

    #[test]
    fn env_readers() {
        // Use a unique var name to avoid clobbering real environment.
        let key = "HERMES_CONFIG_SCHEMA_TEST_VAR";
        std::env::remove_var(key);
        assert_eq!(getenv(key, None), None);
        assert_eq!(getenv(key, Some("d")), Some("d".to_string()));
        assert_eq!(getenv_str(key, "fallback"), "fallback");
        assert_eq!(getenv_int(key, 42), 42);

        std::env::set_var(key, " 17 ");
        assert_eq!(getenv_int(key, 42), 17);
        std::env::set_var(key, "bad");
        assert_eq!(getenv_int(key, 42), 42);
        std::env::remove_var(key);
    }
}
