//! Port of the config dataclasses (HomeChannel, SessionResetPolicy, ChannelOverride, PlatformConfig, StreamingConfig) from gateway/config.py.
//!
// Public API is ahead of its callers while the gateway config pipeline is ported.
#![allow(dead_code)]
//!
//! These are the value dataclasses that hang off `GatewayConfig`. Each becomes a
//! Rust struct with `from_dict` / `to_dict` that mirror the Python round-trip
//! exactly: the same key names, the same defaults, the same coercion rules, and
//! the same "which keys does to_dict emit" logic (some dicts drop falsy/None
//! keys, some always include them).
//!
//! Coercion helpers and the `Platform` enum are reused from `crate::config_schema`
//! (`coerce_bool`, `coerce_int`, `coerce_float`, `coerce_dict`,
//! `normalize_transport_token`, `Platform`). The pure Python `str(...)` and
//! truthiness helpers config_schema keeps private are reproduced privately below
//! (see `py_str` and `py_truthy`).
//!
//! Passthrough note: several Python fields are typed (`str`, `int`) but their
//! `from_dict` does no coercion, it just does `data.get(key)` and stores whatever
//! came in. Those fields are modeled as `serde_json::Value` here so an int, a
//! string, or a float passes through untouched exactly like the Python does.

use serde_json::{json, Map, Value};

use crate::config_schema::{
    coerce_bool, coerce_dict, coerce_float, coerce_int, normalize_transport_token, Platform,
};

// --- Private helpers reproduced from config_schema (kept private there) -------

/// A single shared `Value::Null` so map lookups can hand back a borrow when a
/// key is absent, matching Python's `data.get(key)` returning `None`.
static NULL: Value = Value::Null;

/// `str(value)` for the value shapes these dataclasses actually stringify
/// (chat_id / thread_id / user_id / scope_id). config_schema keeps its own
/// `python_str` private, so this reproduces it. Numbers render like Python
/// (`123`, `1.5`), bools as `True`/`False`.
fn py_str(value: &Value) -> String {
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

/// Python `bool(value)` truthiness. config_schema keeps its `python_bool`
/// private, so this reproduces it: null/false/0/""/[]/{} are falsy.
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

/// `m.get(key)` as a borrow, `&Null` when absent. Mirrors `data.get(key)` where a
/// missing key reads as `None` (and JSON null already stores as `Value::Null`).
fn get<'a>(m: &'a Map<String, Value>, key: &str) -> &'a Value {
    m.get(key).unwrap_or(&NULL)
}

/// `data.get(key)` collapsed to the Python `is not None` test: returns `None`
/// when the key is absent OR present with a JSON null (both read as `None` in
/// Python). Used for the fields whose from_dict does an explicit `is None` check.
fn non_null<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    match m.get(key) {
        Some(Value::Null) | None => None,
        Some(v) => Some(v),
    }
}

// -----------------------------------------------------------------------------
// HomeChannel
// -----------------------------------------------------------------------------

/// Default destination for a platform (port of `HomeChannel`).
///
/// Note on defaults: the Python dataclass makes `platform`, `chat_id`, and
/// `name` required (no field default), so there is no truly faithful zero-value.
/// This `Default` picks placeholders (`Local` / empty chat_id / `"Home"`) so the
/// struct can still be constructed blank; real values always come from
/// `from_dict`.
#[derive(Debug, Clone, PartialEq)]
pub struct HomeChannel {
    pub platform: Platform,
    pub chat_id: String,
    pub name: String,
    pub thread_id: Option<String>,
    pub user_id: Option<String>,
    pub scope_id: Option<String>,
}

impl Default for HomeChannel {
    fn default() -> Self {
        HomeChannel {
            platform: Platform::Local,
            chat_id: String::new(),
            name: "Home".to_string(),
            thread_id: None,
            user_id: None,
            scope_id: None,
        }
    }
}

impl HomeChannel {
    /// Port of `HomeChannel.from_dict`.
    ///
    /// Mirrors Python's required-key indexing: `platform` and `chat_id` must be
    /// present, and `platform` must be a known built-in value. Python raises
    /// KeyError / ValueError in those cases; here we panic with a matching
    /// message (this constructor returns `Self`, not a `Result`). `thread_id` /
    /// `user_id` / `scope_id` are kept only when truthy (empty string, 0, null,
    /// or absent all become `None`).
    pub fn from_dict(data: &Value) -> Self {
        let obj = data
            .as_object()
            .expect("HomeChannel.from_dict expects a JSON object");

        let platform_val = obj
            .get("platform")
            .expect("HomeChannel.from_dict: missing 'platform' key");
        let platform_str = platform_val
            .as_str()
            .expect("HomeChannel.from_dict: 'platform' must be a string");
        let platform = Platform::from_value(platform_str).unwrap_or_else(|| {
            panic!("HomeChannel.from_dict: unknown platform value {platform_str:?}")
        });

        let chat_id_val = obj
            .get("chat_id")
            .expect("HomeChannel.from_dict: missing 'chat_id' key");
        let chat_id = py_str(chat_id_val);

        let name = match obj.get("name") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => py_str(other),
            None => "Home".to_string(),
        };

        let truthy_str = |key: &str| -> Option<String> {
            match obj.get(key) {
                Some(v) if py_truthy(v) => Some(py_str(v)),
                _ => None,
            }
        };

        HomeChannel {
            platform,
            chat_id,
            name,
            thread_id: truthy_str("thread_id"),
            user_id: truthy_str("user_id"),
            scope_id: truthy_str("scope_id"),
        }
    }

    /// Port of `HomeChannel.to_dict`. Always emits `platform` / `chat_id` /
    /// `name`; emits `thread_id` / `user_id` / `scope_id` only when set (they are
    /// only ever `Some` when truthy after from_dict).
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();
        out.insert("platform".to_string(), json!(self.platform.value()));
        out.insert("chat_id".to_string(), json!(self.chat_id));
        out.insert("name".to_string(), json!(self.name));
        if let Some(t) = &self.thread_id {
            out.insert("thread_id".to_string(), json!(t));
        }
        if let Some(u) = &self.user_id {
            out.insert("user_id".to_string(), json!(u));
        }
        if let Some(s) = &self.scope_id {
            out.insert("scope_id".to_string(), json!(s));
        }
        Value::Object(out)
    }
}

// -----------------------------------------------------------------------------
// SessionResetPolicy
// -----------------------------------------------------------------------------

/// Controls when sessions reset (port of `SessionResetPolicy`).
///
/// `mode` / `at_hour` / `idle_minutes` / `bg_process_max_age_hours` are stored as
/// raw `Value` because Python's from_dict does no coercion on them, it just keeps
/// whatever value is present (falling back to the default only for `None`). So a
/// string `at_hour` stays a string on the way back out.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionResetPolicy {
    pub mode: Value,
    pub at_hour: Value,
    pub idle_minutes: Value,
    pub notify: bool,
    pub notify_exclude_platforms: Vec<Value>,
    pub bg_process_max_age_hours: Value,
}

impl Default for SessionResetPolicy {
    fn default() -> Self {
        SessionResetPolicy {
            mode: json!("none"),
            at_hour: json!(4),
            idle_minutes: json!(1440),
            notify: true,
            notify_exclude_platforms: vec![json!("api_server"), json!("webhook")],
            bg_process_max_age_hours: json!(24),
        }
    }
}

impl SessionResetPolicy {
    /// Port of `SessionResetPolicy.from_dict`.
    ///
    /// Every key uses the `value if value is not None else default` rule (so both
    /// a missing key and an explicit YAML null fall back to the default). `notify`
    /// runs through `coerce_bool`. `notify_exclude_platforms` becomes `tuple(...)`
    /// of whatever the value iterates to (see `tuple_of` for the faithful string
    /// / dict iteration behavior).
    pub fn from_dict(data: &Value) -> Self {
        let obj = coerce_dict(data);

        let mode = non_null(&obj, "mode")
            .cloned()
            .unwrap_or_else(|| json!("none"));
        let at_hour = non_null(&obj, "at_hour")
            .cloned()
            .unwrap_or_else(|| json!(4));
        let idle_minutes = non_null(&obj, "idle_minutes")
            .cloned()
            .unwrap_or_else(|| json!(1440));
        let notify = coerce_bool(get(&obj, "notify"), true);
        let notify_exclude_platforms = match non_null(&obj, "notify_exclude_platforms") {
            Some(v) => tuple_of(v),
            None => vec![json!("api_server"), json!("webhook")],
        };
        let bg_process_max_age_hours = non_null(&obj, "bg_process_max_age_hours")
            .cloned()
            .unwrap_or_else(|| json!(24));

        SessionResetPolicy {
            mode,
            at_hour,
            idle_minutes,
            notify,
            notify_exclude_platforms,
            bg_process_max_age_hours,
        }
    }

    /// Port of `SessionResetPolicy.to_dict`. Always emits all six keys.
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();
        out.insert("mode".to_string(), self.mode.clone());
        out.insert("at_hour".to_string(), self.at_hour.clone());
        out.insert("idle_minutes".to_string(), self.idle_minutes.clone());
        out.insert("notify".to_string(), json!(self.notify));
        out.insert(
            "notify_exclude_platforms".to_string(),
            Value::Array(self.notify_exclude_platforms.clone()),
        );
        out.insert(
            "bg_process_max_age_hours".to_string(),
            self.bg_process_max_age_hours.clone(),
        );
        Value::Object(out)
    }
}

/// Python `tuple(value)` for the value shapes a config can hold. A list yields
/// its elements; a string yields its characters; a dict yields its keys. Numbers
/// / bools are non-iterable in Python (`tuple(5)` raises TypeError), so we panic
/// to mirror that. `null` never reaches here (handled by the `is not None` guard
/// upstream).
fn tuple_of(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::String(s) => s.chars().map(|c| json!(c.to_string())).collect(),
        Value::Object(map) => map.keys().map(|k| json!(k)).collect(),
        other => panic!("tuple() argument is not iterable: {other:?}"),
    }
}

// -----------------------------------------------------------------------------
// ChannelOverride
// -----------------------------------------------------------------------------

/// Per-channel model / provider / system_prompt override (port of
/// `ChannelOverride`). Fields are `Option<Value>` (not `Option<String>`) because
/// Python's from_dict does no coercion, it passes `data.get(key)` straight
/// through; a JSON null reads as `None` (absent) and is dropped by to_dict.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelOverride {
    pub model: Option<Value>,
    pub provider: Option<Value>,
    pub system_prompt: Option<Value>,
}

impl ChannelOverride {
    /// Port of `ChannelOverride.from_dict`. A falsy dict (empty or non-object)
    /// yields the all-`None` default; otherwise each field is `data.get(key)`
    /// with a JSON null collapsing to `None`.
    pub fn from_dict(data: &Value) -> Self {
        let obj = match data.as_object() {
            Some(o) if !o.is_empty() => o,
            _ => return ChannelOverride::default(),
        };
        ChannelOverride {
            model: non_null(obj, "model").cloned(),
            provider: non_null(obj, "provider").cloned(),
            system_prompt: non_null(obj, "system_prompt").cloned(),
        }
    }

    /// Port of `ChannelOverride.to_dict`. Emits a key only when it `is not None`.
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();
        if let Some(v) = &self.model {
            out.insert("model".to_string(), v.clone());
        }
        if let Some(v) = &self.provider {
            out.insert("provider".to_string(), v.clone());
        }
        if let Some(v) = &self.system_prompt {
            out.insert("system_prompt".to_string(), v.clone());
        }
        Value::Object(out)
    }
}

// -----------------------------------------------------------------------------
// PlatformConfig
// -----------------------------------------------------------------------------

/// Configuration for a single messaging platform (port of `PlatformConfig`).
///
/// `token` / `api_key` / `typing_status_text` are `Option<Value>` passthroughs
/// (no coercion in Python). `reply_to_mode` is a `Value` because its from_dict
/// uses `data.get("reply_to_mode", "first")`, which keeps an explicit null (the
/// default only fires when the key is entirely absent). `channel_overrides` is a
/// BTreeMap so to_dict emits sorted keys, matching Python's `json.dumps(...,
/// sort_keys=True)` on the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct PlatformConfig {
    pub enabled: bool,
    pub token: Option<Value>,
    pub api_key: Option<Value>,
    pub home_channel: Option<HomeChannel>,
    pub reply_to_mode: Value,
    pub gateway_restart_notification: bool,
    pub typing_indicator: bool,
    pub typing_status_text: Option<Value>,
    pub channel_overrides: std::collections::BTreeMap<String, ChannelOverride>,
    pub extra: Map<String, Value>,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        PlatformConfig {
            enabled: false,
            token: None,
            api_key: None,
            home_channel: None,
            reply_to_mode: json!("first"),
            gateway_restart_notification: true,
            typing_indicator: true,
            typing_status_text: None,
            channel_overrides: std::collections::BTreeMap::new(),
            extra: Map::new(),
        }
    }
}

impl PlatformConfig {
    /// Port of `PlatformConfig.from_dict`.
    ///
    /// Subtleties preserved:
    /// - `gateway_restart_notification` and `typing_indicator` may arrive either
    ///   top-level or bridged into `extra` by the load_gateway_config shared-key
    ///   loop, so a `None` at top level falls back to the `extra` copy before
    ///   `coerce_bool(..., True)`.
    /// - `typing_status_text` takes the same two routes but is a string
    ///   passthrough (no coercion).
    /// - `reply_to_mode` keeps an explicit null; only an absent key defaults to
    ///   "first".
    /// - malformed `channel_overrides` entries (non-dict values) are skipped.
    pub fn from_dict(data: &Value) -> Self {
        let obj = coerce_dict(data);

        let home_channel = match obj.get("home_channel") {
            Some(hc) if hc.is_object() => Some(HomeChannel::from_dict(hc)),
            _ => None,
        };

        let extra = coerce_dict(get(&obj, "extra"));

        // gateway_restart_notification: top-level, else bridged into extra.
        let grn = non_null(&obj, "gateway_restart_notification")
            .or_else(|| non_null(&extra, "gateway_restart_notification"));
        let gateway_restart_notification = coerce_bool(grn.unwrap_or(&NULL), true);

        // typing_indicator: same top-level-or-extra bridge.
        let typing =
            non_null(&obj, "typing_indicator").or_else(|| non_null(&extra, "typing_indicator"));
        let typing_indicator = coerce_bool(typing.unwrap_or(&NULL), true);

        // typing_status_text: same routes, string passthrough (no coercion).
        let typing_status_text = non_null(&obj, "typing_status_text")
            .or_else(|| non_null(&extra, "typing_status_text"))
            .cloned();

        let mut channel_overrides = std::collections::BTreeMap::new();
        if let Some(Value::Object(raw)) = obj.get("channel_overrides") {
            for (cid, ov) in raw {
                if ov.is_object() {
                    channel_overrides.insert(cid.clone(), ChannelOverride::from_dict(ov));
                }
            }
        }

        // reply_to_mode uses .get(key, "first"): present-null stays null, only an
        // absent key defaults.
        let reply_to_mode = obj
            .get("reply_to_mode")
            .cloned()
            .unwrap_or_else(|| json!("first"));

        PlatformConfig {
            enabled: coerce_bool(get(&obj, "enabled"), false),
            token: non_null(&obj, "token").cloned(),
            api_key: non_null(&obj, "api_key").cloned(),
            home_channel,
            reply_to_mode,
            gateway_restart_notification,
            typing_indicator,
            typing_status_text,
            channel_overrides,
            extra,
        }
    }

    /// Port of `PlatformConfig.to_dict`.
    ///
    /// Always emits `enabled` / `extra` / `reply_to_mode` /
    /// `gateway_restart_notification` / `typing_indicator`. Emits
    /// `typing_status_text` when it `is not None`; `token` / `api_key` only when
    /// truthy (empty string is dropped); `home_channel` when set;
    /// `channel_overrides` only when non-empty.
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();
        out.insert("enabled".to_string(), json!(self.enabled));
        out.insert("extra".to_string(), Value::Object(self.extra.clone()));
        out.insert("reply_to_mode".to_string(), self.reply_to_mode.clone());
        out.insert(
            "gateway_restart_notification".to_string(),
            json!(self.gateway_restart_notification),
        );
        out.insert("typing_indicator".to_string(), json!(self.typing_indicator));

        if let Some(t) = &self.typing_status_text {
            out.insert("typing_status_text".to_string(), t.clone());
        }
        if let Some(t) = &self.token {
            if py_truthy(t) {
                out.insert("token".to_string(), t.clone());
            }
        }
        if let Some(k) = &self.api_key {
            if py_truthy(k) {
                out.insert("api_key".to_string(), k.clone());
            }
        }
        if let Some(hc) = &self.home_channel {
            out.insert("home_channel".to_string(), hc.to_dict());
        }
        if !self.channel_overrides.is_empty() {
            let mut co = Map::new();
            for (cid, ov) in &self.channel_overrides {
                co.insert(cid.clone(), ov.to_dict());
            }
            out.insert("channel_overrides".to_string(), Value::Object(co));
        }
        Value::Object(out)
    }
}

// -----------------------------------------------------------------------------
// StreamingConfig
// -----------------------------------------------------------------------------

/// Streaming edit-rhythm defaults, the single source of truth shared with the
/// Python module.
pub const DEFAULT_STREAMING_EDIT_INTERVAL: f64 = 0.8;
pub const DEFAULT_STREAMING_BUFFER_THRESHOLD: i64 = 24;
pub const DEFAULT_STREAMING_CURSOR: &str = " \u{2589}";

/// Configuration for real-time token streaming (port of `StreamingConfig`).
///
/// `cursor` is a `Value` because its from_dict uses `data.get("cursor",
/// DEFAULT)`, so an explicit null is kept and only an absent key gets the
/// default.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamingConfig {
    pub enabled: bool,
    pub transport: String,
    pub edit_interval: f64,
    pub buffer_threshold: i64,
    pub cursor: Value,
    pub fresh_final_after_seconds: f64,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        StreamingConfig {
            enabled: false,
            transport: "auto".to_string(),
            edit_interval: DEFAULT_STREAMING_EDIT_INTERVAL,
            buffer_threshold: DEFAULT_STREAMING_BUFFER_THRESHOLD,
            cursor: json!(DEFAULT_STREAMING_CURSOR),
            fresh_final_after_seconds: 0.0,
        }
    }
}

impl StreamingConfig {
    /// Port of `StreamingConfig.from_dict`.
    ///
    /// A non-object or empty object yields the default. `transport` prefers the
    /// `transport` key, else the `mode` alias, normalized through
    /// `normalize_transport_token` (so YAML's bare on/off maps to auto/off).
    /// `enabled` resolution: an explicit `enabled` key wins (via `coerce_bool`);
    /// otherwise the `mode` alias (and only `mode`) infers it (anything but
    /// "off" enables); a bare `transport` never flips `enabled`.
    pub fn from_dict(data: &Value) -> Self {
        let obj = match data.as_object() {
            Some(o) if !o.is_empty() => o,
            _ => return StreamingConfig::default(),
        };

        let raw_transport = non_null(obj, "transport");
        let raw_mode = non_null(obj, "mode");
        let picked = raw_transport.or(raw_mode).unwrap_or(&NULL);
        let transport = normalize_transport_token(picked);

        let enabled = if obj.contains_key("enabled") {
            coerce_bool(get(obj, "enabled"), false)
        } else if let Some(mode) = raw_mode {
            normalize_transport_token(mode) != "off"
        } else {
            false
        };

        let cursor = obj
            .get("cursor")
            .cloned()
            .unwrap_or_else(|| json!(DEFAULT_STREAMING_CURSOR));

        StreamingConfig {
            enabled,
            transport,
            edit_interval: coerce_float(get(obj, "edit_interval"), DEFAULT_STREAMING_EDIT_INTERVAL),
            buffer_threshold: coerce_int(
                get(obj, "buffer_threshold"),
                DEFAULT_STREAMING_BUFFER_THRESHOLD,
            ),
            cursor,
            fresh_final_after_seconds: coerce_float(get(obj, "fresh_final_after_seconds"), 0.0),
        }
    }

    /// Port of `StreamingConfig.to_dict`. Always emits all six keys.
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();
        out.insert("enabled".to_string(), json!(self.enabled));
        out.insert("transport".to_string(), json!(self.transport));
        out.insert("edit_interval".to_string(), json!(self.edit_interval));
        out.insert("buffer_threshold".to_string(), json!(self.buffer_threshold));
        out.insert("cursor".to_string(), self.cursor.clone());
        out.insert(
            "fresh_final_after_seconds".to_string(),
            json!(self.fresh_final_after_seconds),
        );
        Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fixture strings use Python's explicit sort_keys option.
    fn dumps(v: &Value) -> String {
        let mut value = v.clone();
        value.sort_all_objects();
        value.to_string()
    }

    /// Golden helper: from_dict(input).to_dict() must serialize to `expected`.
    fn golden<F>(from: F, input: Value, expected: &str)
    where
        F: Fn(&Value) -> Value,
    {
        let got = from(&input);
        assert_eq!(dumps(&got), expected, "input was {input}");
    }

    // ---------------------------------------------------------------- HomeChannel

    #[test]
    fn home_channel_full() {
        golden(
            |v| HomeChannel::from_dict(v).to_dict(),
            json!({
                "platform":"telegram","chat_id":123,"name":"My Home",
                "thread_id":"t1","user_id":"u9","scope_id":"s2"}),
            r#"{"chat_id":"123","name":"My Home","platform":"telegram","scope_id":"s2","thread_id":"t1","user_id":"u9"}"#,
        );
    }

    #[test]
    fn home_channel_min_defaults_name() {
        golden(
            |v| HomeChannel::from_dict(v).to_dict(),
            json!({"platform":"discord","chat_id":"c-1"}),
            r#"{"chat_id":"c-1","name":"Home","platform":"discord"}"#,
        );
    }

    #[test]
    fn home_channel_falsy_optionals_dropped() {
        // thread_id "" (empty), user_id 0, scope_id null are all falsy -> omitted.
        golden(
            |v| HomeChannel::from_dict(v).to_dict(),
            json!({"platform":"slack","chat_id":"c","name":"N",
                   "thread_id":"","user_id":0,"scope_id":null}),
            r#"{"chat_id":"c","name":"N","platform":"slack"}"#,
        );
    }

    #[test]
    #[should_panic(expected = "unknown platform")]
    fn home_channel_unknown_platform_panics() {
        let _ = HomeChannel::from_dict(&json!({"platform":"irc","chat_id":"x"}));
    }

    #[test]
    #[should_panic(expected = "missing 'chat_id'")]
    fn home_channel_missing_chat_id_panics() {
        let _ = HomeChannel::from_dict(&json!({"platform":"telegram"}));
    }

    // -------------------------------------------------------- SessionResetPolicy

    #[test]
    fn srp_default_empty() {
        golden(
            |v| SessionResetPolicy::from_dict(v).to_dict(),
            json!({}),
            r#"{"at_hour":4,"bg_process_max_age_hours":24,"idle_minutes":1440,"mode":"none","notify":true,"notify_exclude_platforms":["api_server","webhook"]}"#,
        );
    }

    #[test]
    fn srp_full() {
        // notify "no" -> coerce_bool false.
        golden(
            |v| SessionResetPolicy::from_dict(v).to_dict(),
            json!({"mode":"idle","at_hour":6,"idle_minutes":30,"notify":"no",
                   "notify_exclude_platforms":["slack","discord"],"bg_process_max_age_hours":48}),
            r#"{"at_hour":6,"bg_process_max_age_hours":48,"idle_minutes":30,"mode":"idle","notify":false,"notify_exclude_platforms":["slack","discord"]}"#,
        );
    }

    #[test]
    fn srp_nulls_fall_back_to_defaults() {
        golden(
            |v| SessionResetPolicy::from_dict(v).to_dict(),
            json!({"mode":null,"at_hour":null,"idle_minutes":null,"notify":null,
                   "notify_exclude_platforms":null,"bg_process_max_age_hours":null}),
            r#"{"at_hour":4,"bg_process_max_age_hours":24,"idle_minutes":1440,"mode":"none","notify":true,"notify_exclude_platforms":["api_server","webhook"]}"#,
        );
    }

    #[test]
    fn srp_passthrough_types_uncoerced() {
        // mode/at_hour/idle_minutes/bg are stored raw: int 5, "7", "x", 1.5.
        golden(
            |v| SessionResetPolicy::from_dict(v).to_dict(),
            json!({"mode":5,"at_hour":"7","idle_minutes":"x","bg_process_max_age_hours":1.5}),
            r#"{"at_hour":"7","bg_process_max_age_hours":1.5,"idle_minutes":"x","mode":5,"notify":true,"notify_exclude_platforms":["api_server","webhook"]}"#,
        );
    }

    #[test]
    fn srp_empty_exclude_kept() {
        // Present empty list is kept (tuple([]) == ()), not defaulted.
        golden(
            |v| SessionResetPolicy::from_dict(v).to_dict(),
            json!({"notify_exclude_platforms":[]}),
            r#"{"at_hour":4,"bg_process_max_age_hours":24,"idle_minutes":1440,"mode":"none","notify":true,"notify_exclude_platforms":[]}"#,
        );
    }

    // ------------------------------------------------------------ ChannelOverride

    #[test]
    fn channel_override_empty() {
        golden(
            |v| ChannelOverride::from_dict(v).to_dict(),
            json!({}),
            r#"{}"#,
        );
    }

    #[test]
    fn channel_override_full() {
        golden(
            |v| ChannelOverride::from_dict(v).to_dict(),
            json!({"model":"m","provider":"p","system_prompt":"s"}),
            r#"{"model":"m","provider":"p","system_prompt":"s"}"#,
        );
    }

    #[test]
    fn channel_override_partial_null_dropped() {
        // provider null -> None -> omitted.
        golden(
            |v| ChannelOverride::from_dict(v).to_dict(),
            json!({"model":"m","provider":null}),
            r#"{"model":"m"}"#,
        );
    }

    // ------------------------------------------------------------- PlatformConfig

    #[test]
    fn platform_config_default() {
        golden(
            |v| PlatformConfig::from_dict(v).to_dict(),
            json!({}),
            r#"{"enabled":false,"extra":{},"gateway_restart_notification":true,"reply_to_mode":"first","typing_indicator":true}"#,
        );
    }

    #[test]
    fn platform_config_full() {
        golden(
            |v| PlatformConfig::from_dict(v).to_dict(),
            json!({
                "enabled":"yes","token":"tok","api_key":"key",
                "home_channel":{"platform":"telegram","chat_id":"h","name":"H"},
                "reply_to_mode":"all","gateway_restart_notification":"off",
                "typing_indicator":false,"typing_status_text":"working...",
                "channel_overrides":{"c1":{"model":"m1"},"c2":{"provider":"p2"}},
                "extra":{"foo":"bar"}}),
            r#"{"api_key":"key","channel_overrides":{"c1":{"model":"m1"},"c2":{"provider":"p2"}},"enabled":true,"extra":{"foo":"bar"},"gateway_restart_notification":false,"home_channel":{"chat_id":"h","name":"H","platform":"telegram"},"reply_to_mode":"all","token":"tok","typing_indicator":false,"typing_status_text":"working..."}"#,
        );
    }

    #[test]
    fn platform_config_extra_bridge() {
        // grn / typing / typing_status_text bridged in from extra.
        golden(
            |v| PlatformConfig::from_dict(v).to_dict(),
            json!({"extra":{"gateway_restart_notification":false,
                            "typing_indicator":false,"typing_status_text":"t"}}),
            r#"{"enabled":false,"extra":{"gateway_restart_notification":false,"typing_indicator":false,"typing_status_text":"t"},"gateway_restart_notification":false,"reply_to_mode":"first","typing_indicator":false,"typing_status_text":"t"}"#,
        );
    }

    #[test]
    fn platform_config_empty_token_dropped() {
        // Empty-string token / api_key are falsy -> omitted.
        golden(
            |v| PlatformConfig::from_dict(v).to_dict(),
            json!({"token":"","api_key":""}),
            r#"{"enabled":false,"extra":{},"gateway_restart_notification":true,"reply_to_mode":"first","typing_indicator":true}"#,
        );
    }

    #[test]
    fn platform_config_bad_overrides_skipped() {
        // Non-dict override entry "c1" is dropped; "c2" kept.
        golden(
            |v| PlatformConfig::from_dict(v).to_dict(),
            json!({"channel_overrides":{"c1":"notadict","c2":{"model":"ok"}}}),
            r#"{"channel_overrides":{"c2":{"model":"ok"}},"enabled":false,"extra":{},"gateway_restart_notification":true,"reply_to_mode":"first","typing_indicator":true}"#,
        );
    }

    // ------------------------------------------------------------- StreamingConfig

    #[test]
    fn streaming_default_empty() {
        golden(
            |v| StreamingConfig::from_dict(v).to_dict(),
            json!({}),
            r#"{"buffer_threshold":24,"cursor":" ▉","edit_interval":0.8,"enabled":false,"fresh_final_after_seconds":0.0,"transport":"auto"}"#,
        );
    }

    #[test]
    fn streaming_struct_default_matches() {
        assert_eq!(
            dumps(&StreamingConfig::default().to_dict()),
            r#"{"buffer_threshold":24,"cursor":" ▉","edit_interval":0.8,"enabled":false,"fresh_final_after_seconds":0.0,"transport":"auto"}"#,
        );
    }

    #[test]
    fn streaming_mode_alias_enables() {
        // mode alias turns streaming on and picks the transport.
        golden(
            |v| StreamingConfig::from_dict(v).to_dict(),
            json!({"mode":"draft"}),
            r#"{"buffer_threshold":24,"cursor":" ▉","edit_interval":0.8,"enabled":true,"fresh_final_after_seconds":0.0,"transport":"draft"}"#,
        );
    }

    #[test]
    fn streaming_mode_off_disables() {
        golden(
            |v| StreamingConfig::from_dict(v).to_dict(),
            json!({"mode":"off"}),
            r#"{"buffer_threshold":24,"cursor":" ▉","edit_interval":0.8,"enabled":false,"fresh_final_after_seconds":0.0,"transport":"off"}"#,
        );
    }

    #[test]
    fn streaming_transport_only_no_enable() {
        // Bare transport selects HOW but does not enable.
        golden(
            |v| StreamingConfig::from_dict(v).to_dict(),
            json!({"transport":"edit"}),
            r#"{"buffer_threshold":24,"cursor":" ▉","edit_interval":0.8,"enabled":false,"fresh_final_after_seconds":0.0,"transport":"edit"}"#,
        );
    }

    #[test]
    fn streaming_full_coercions() {
        // string numbers coerce; fresh 3 -> 3.0 float.
        golden(
            |v| StreamingConfig::from_dict(v).to_dict(),
            json!({"enabled":"true","transport":"edit","edit_interval":"1.2",
                   "buffer_threshold":"40","cursor":"X","fresh_final_after_seconds":3}),
            r#"{"buffer_threshold":40,"cursor":"X","edit_interval":1.2,"enabled":true,"fresh_final_after_seconds":3.0,"transport":"edit"}"#,
        );
    }

    #[test]
    fn streaming_explicit_enabled_overrides_mode() {
        // enabled:false wins even though mode:draft would enable.
        golden(
            |v| StreamingConfig::from_dict(v).to_dict(),
            json!({"enabled":false,"mode":"draft"}),
            r#"{"buffer_threshold":24,"cursor":" ▉","edit_interval":0.8,"enabled":false,"fresh_final_after_seconds":0.0,"transport":"draft"}"#,
        );
    }

    #[test]
    fn streaming_yaml_on_bool_mode() {
        // YAML bare `on` parses to bool true -> transport "auto", enabled true.
        golden(
            |v| StreamingConfig::from_dict(v).to_dict(),
            json!({"mode":true}),
            r#"{"buffer_threshold":24,"cursor":" ▉","edit_interval":0.8,"enabled":true,"fresh_final_after_seconds":0.0,"transport":"auto"}"#,
        );
    }

    #[test]
    fn streaming_bad_floats_fall_back() {
        golden(
            |v| StreamingConfig::from_dict(v).to_dict(),
            json!({"enabled":true,"edit_interval":"bad","buffer_threshold":"bad"}),
            r#"{"buffer_threshold":24,"cursor":" ▉","edit_interval":0.8,"enabled":true,"fresh_final_after_seconds":0.0,"transport":"auto"}"#,
        );
    }
}
