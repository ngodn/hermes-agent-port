//! Port of the GatewayConfig dataclass from gateway/config.py.
//!
// Public API is ahead of its callers while the gateway config pipeline is ported.
#![allow(dead_code)]
//!
//! This is the top-level gateway config aggregate: the struct fields and their
//! defaults, `__post_init__`, `to_dict`, `from_dict`, plus the standalone helper
//! `_has_usable_api_server_key`. The three impure loaders that sit above it in
//! Python (`load_gateway_config`, `_validate_gateway_config`,
//! `_apply_env_overrides`, which touch YAML / secrets / env) are ported
//! separately.
//!
//! Reused wholesale:
//! - `crate::config_schema`: the `Platform` enum and the value coercers
//!   (`coerce_bool`, `coerce_int`, `coerce_float`, `coerce_optional_positive_int`,
//!   `coerce_dict`, `coerce_systemd_watchdog_seconds`,
//!   `normalize_unauthorized_dm_behavior`, `normalize_multiplex_profile_allowlist`,
//!   `env_multiplex_profiles_override`).
//! - `crate::config_types`: `PlatformConfig`, `SessionResetPolicy`,
//!   `StreamingConfig` (each with `from_dict` / `to_dict` / `Default`).
//! - `crate::profile_routing`: `ProfileRoute` + `parse_profile_routes`.
//! - `crate::config_file::hermes_home`: the sessions_dir default root.
//!
//! Python `Dict[str, Any]` fields with no coercion (`quick_commands`,
//! `reset_triggers`) become `serde_json::Value` / `Map` so any shape passes
//! through untouched, exactly like the sibling value dataclasses in
//! `config_types.rs`.
//!
//! Two divergences from Python worth flagging up front, both inherited from
//! `config_schema::Platform` and documented there:
//! - `Platform(name)` in Python mints pseudo-members for filesystem-discovered
//!   plugin platforms (`irc`, `line`, `teams`, ...) via `_missing_`. That
//!   registry/filesystem scan is not ported, so a platform key that is not a
//!   built-in value is skipped here (Python would keep a discovered plugin, but
//!   still raises ValueError -> skips a genuinely-unknown name, which is the
//!   branch we reproduce). We do reproduce the `_missing_` strip+lowercase
//!   normalization for the built-in tier, so `"Telegram"` / `" telegram "` still
//!   resolve to `Platform::Telegram`.
//! - built-in value lookup is therefore case-insensitive here (matching Python),
//!   even though `config_schema::Platform::from_value` on its own is
//!   case-sensitive; `parse_platform_key` does the strip+lower first.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::config_file::hermes_home;
use crate::config_schema::{
    coerce_bool, coerce_dict, coerce_float, coerce_int, coerce_optional_positive_int,
    coerce_systemd_watchdog_seconds, env_multiplex_profiles_override,
    normalize_multiplex_profile_allowlist, normalize_unauthorized_dm_behavior, Platform,
};
use crate::config_types::{PlatformConfig, SessionResetPolicy, StreamingConfig};
use crate::profile_routing::{parse_profile_routes, ProfileRoute};

// --- Loop-watchdog defaults -------------------------------------------------
//
// In Python these live in `gateway/shutdown_watchdog.py` and are imported into
// `gateway/config.py`. That module is not ported yet, so the constants are
// reproduced here (single source of truth once shutdown_watchdog lands).
pub const DEFAULT_LOOP_WATCHDOG_INTERVAL_S: f64 = 30.0;
pub const DEFAULT_LOOP_WATCHDOG_TIMEOUT_S: f64 = 10.0;
pub const DEFAULT_LOOP_WATCHDOG_MAX_STRIKES: i64 = 3;

// --- Private helpers ---------------------------------------------------------

/// A single shared `Value::Null` so map lookups can hand back a borrow when a
/// key is absent, matching Python's `data.get(key)` returning `None`.
static NULL: Value = Value::Null;

/// `m.get(key)` as a borrow, `&Null` when absent. Mirrors `data.get(key)`.
fn get<'a>(m: &'a Map<String, Value>, key: &str) -> &'a Value {
    m.get(key).unwrap_or(&NULL)
}

/// `str(value)` for the shapes a sessions_dir / id field actually holds.
/// config_schema keeps its own stringifier private, so this reproduces it.
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

/// Python truthiness `bool(value)` (null/false/0/""/[]/{} are falsy).
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

/// The `X if "key" in data else nested.get("key")` pattern used across the
/// watchdog / multiplex / max-concurrent fields. Presence-based: a top-level key
/// present with an explicit null wins over the nested value (Python `in` check).
fn key_or_nested<'a>(
    data: &'a Map<String, Value>,
    nested: &'a Map<String, Value>,
    key: &str,
) -> &'a Value {
    if data.contains_key(key) {
        data.get(key).unwrap_or(&NULL)
    } else {
        nested.get(key).unwrap_or(&NULL)
    }
}

/// Reproduce `Platform(name)` for the built-in tier: strip + lowercase (as
/// `_missing_` does) then match a built-in `.value`. Empty/whitespace and any
/// non-built-in (including plugin platforms this port can't discover) return
/// `None`, i.e. the Python `except ValueError: pass` skip branch.
fn parse_platform_key(name: &str) -> Option<Platform> {
    let normalized = name.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    Platform::from_value(&normalized)
}

/// Port of the `asdict(ProfileRoute)` step in `to_dict`. Emits all seven fields
/// in every entry; unset optionals render as JSON null (matching dataclass
/// `asdict`).
fn profile_route_to_dict(r: &ProfileRoute) -> Value {
    let opt = |o: &Option<String>| match o {
        Some(s) => json!(s),
        None => Value::Null,
    };
    json!({
        "name": r.name,
        "platform": r.platform,
        "profile": r.profile,
        "guild_id": opt(&r.guild_id),
        "chat_id": opt(&r.chat_id),
        "thread_id": opt(&r.thread_id),
        "enabled": r.enabled,
    })
}

// --- _has_usable_api_server_key ---------------------------------------------

/// Placeholder tokens rejected by `hermes_cli.auth._PLACEHOLDER_SECRET_VALUES`
/// (compared case-folded).
const PLACEHOLDER_SECRET_VALUES: &[&str] = &[
    "*",
    "**",
    "***",
    "changeme",
    "dummy",
    "example",
    "none",
    "null",
    "placeholder",
    "your-api-key",
    "your_api_key",
    "your_api_key_here",
];

/// Port of `hermes_cli.auth.has_usable_secret` for the `min_length = 16` call
/// site. Non-string -> False; stripped length below the minimum -> False;
/// case-folded placeholder -> False; else True.
fn has_usable_secret(value: &Value, min_length: usize) -> bool {
    let s = match value {
        Value::String(s) => s,
        _ => return false,
    };
    let cleaned = s.trim();
    if cleaned.chars().count() < min_length {
        return false;
    }
    if PLACEHOLDER_SECRET_VALUES.contains(&cleaned.to_lowercase().as_str()) {
        return false;
    }
    true
}

/// Port of `_has_usable_api_server_key`.
///
/// `if not key: return False` runs Python truthiness first (None/""/0/[]/{} all
/// fail), then defers to `has_usable_secret(key, min_length=16)`. A non-string
/// truthy value (e.g. an int) passes the truthiness guard but fails
/// `has_usable_secret`'s `isinstance(value, str)` check, so it is unusable. The
/// ImportError fallback in Python (`len(str(key).strip()) >= 16`) only fires when
/// `hermes_cli.auth` is missing; the primary path is what runs, so that is what
/// we port.
pub fn has_usable_api_server_key(key: &Value) -> bool {
    if !py_truthy(key) {
        return false;
    }
    has_usable_secret(key, 16)
}

// --- GatewayConfig -----------------------------------------------------------

/// Main gateway configuration (port of `GatewayConfig`).
///
/// Field notes on the passthrough / raw-Value fields:
/// - `reset_triggers` is a `Value` because Python's from_dict does
///   `data.get("reset_triggers", ["/new", "/reset"])` with no coercion: an
///   explicit null is kept, only an absent key defaults, and a non-list is
///   emitted back unchanged.
/// - `quick_commands` is a `Map` coerced to `{}` when absent or non-dict.
/// - `platforms` / `reset_by_platform` are keyed by `Platform`;
///   `reset_by_type` by the raw string type name.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayConfig {
    pub platforms: HashMap<Platform, PlatformConfig>,
    pub default_reset_policy: SessionResetPolicy,
    pub reset_by_type: BTreeMap<String, SessionResetPolicy>,
    pub reset_by_platform: HashMap<Platform, SessionResetPolicy>,
    pub reset_triggers: Value,
    pub quick_commands: Map<String, Value>,
    pub sessions_dir: PathBuf,
    pub write_sessions_json: bool,
    pub always_log_local: bool,
    pub filter_silence_narration: bool,
    pub stt_enabled: bool,
    pub stt_echo_transcripts: bool,
    pub group_sessions_per_user: bool,
    pub thread_sessions_per_user: bool,
    pub max_concurrent_sessions: Option<i64>,
    pub multiplex_profiles: bool,
    pub multiplex_profile_allowlist: Option<Vec<String>>,
    pub room_link_url: Option<String>,
    pub systemd_watchdog_seconds: i64,
    pub loop_watchdog: bool,
    pub loop_watchdog_probe_interval_s: f64,
    pub loop_watchdog_probe_timeout_s: f64,
    pub loop_watchdog_max_strikes: i64,
    pub unauthorized_dm_behavior: String,
    pub streaming: StreamingConfig,
    pub session_store_max_age_days: i64,
    pub profile_routes: Vec<ProfileRoute>,
}

impl Default for GatewayConfig {
    /// Matches every dataclass field default. `sessions_dir` reads
    /// `hermes_home()/"sessions"` exactly like the Python `default_factory`.
    fn default() -> Self {
        GatewayConfig {
            platforms: HashMap::new(),
            default_reset_policy: SessionResetPolicy::default(),
            reset_by_type: BTreeMap::new(),
            reset_by_platform: HashMap::new(),
            reset_triggers: json!(["/new", "/reset"]),
            quick_commands: Map::new(),
            sessions_dir: hermes_home().join("sessions"),
            write_sessions_json: true,
            always_log_local: true,
            filter_silence_narration: true,
            stt_enabled: true,
            stt_echo_transcripts: true,
            group_sessions_per_user: true,
            thread_sessions_per_user: false,
            max_concurrent_sessions: None,
            multiplex_profiles: false,
            multiplex_profile_allowlist: None,
            room_link_url: None,
            systemd_watchdog_seconds: 0,
            loop_watchdog: true,
            loop_watchdog_probe_interval_s: DEFAULT_LOOP_WATCHDOG_INTERVAL_S,
            loop_watchdog_probe_timeout_s: DEFAULT_LOOP_WATCHDOG_TIMEOUT_S,
            loop_watchdog_max_strikes: DEFAULT_LOOP_WATCHDOG_MAX_STRIKES,
            unauthorized_dm_behavior: "pair".to_string(),
            streaming: StreamingConfig::default(),
            session_store_max_age_days: 90,
            profile_routes: Vec::new(),
        }
    }
}

impl GatewayConfig {
    /// Port of `__post_init__`.
    ///
    /// Python normalizes two fields after the dataclass assigns them:
    /// `multiplex_profile_allowlist` through
    /// `_normalize_multiplex_profile_allowlist`, and `systemd_watchdog_seconds`
    /// through `coerce_systemd_watchdog_seconds`. Because the typed struct can't
    /// hold the raw allowlist value, `from_dict` passes it in explicitly here;
    /// that mirrors Python reading `self.multiplex_profile_allowlist` (which at
    /// `__post_init__` time still holds the raw value). The systemd re-coercion
    /// is idempotent (from_dict already coerced once, and Python does too).
    fn post_init(&mut self, raw_allowlist: &Value) {
        self.multiplex_profile_allowlist = normalize_multiplex_profile_allowlist(raw_allowlist);
        self.systemd_watchdog_seconds =
            coerce_systemd_watchdog_seconds(&json!(self.systemd_watchdog_seconds));
    }

    /// Port of `GatewayConfig.to_dict`. Emits the exact key set the Python
    /// version does, always including every scalar field; `sessions_dir` is
    /// stringified; nested dataclasses defer to their own `to_dict`.
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();

        let mut platforms = Map::new();
        for (p, c) in &self.platforms {
            platforms.insert(p.value().to_string(), c.to_dict());
        }
        out.insert("platforms".to_string(), Value::Object(platforms));

        out.insert(
            "default_reset_policy".to_string(),
            self.default_reset_policy.to_dict(),
        );

        let mut rbt = Map::new();
        for (k, v) in &self.reset_by_type {
            rbt.insert(k.clone(), v.to_dict());
        }
        out.insert("reset_by_type".to_string(), Value::Object(rbt));

        let mut rbp = Map::new();
        for (p, v) in &self.reset_by_platform {
            rbp.insert(p.value().to_string(), v.to_dict());
        }
        out.insert("reset_by_platform".to_string(), Value::Object(rbp));

        out.insert("reset_triggers".to_string(), self.reset_triggers.clone());
        out.insert(
            "quick_commands".to_string(),
            Value::Object(self.quick_commands.clone()),
        );
        out.insert(
            "sessions_dir".to_string(),
            json!(self.sessions_dir.to_string_lossy().to_string()),
        );
        out.insert(
            "write_sessions_json".to_string(),
            json!(self.write_sessions_json),
        );
        out.insert("always_log_local".to_string(), json!(self.always_log_local));
        out.insert(
            "filter_silence_narration".to_string(),
            json!(self.filter_silence_narration),
        );
        out.insert("stt_enabled".to_string(), json!(self.stt_enabled));
        out.insert(
            "stt_echo_transcripts".to_string(),
            json!(self.stt_echo_transcripts),
        );
        out.insert(
            "group_sessions_per_user".to_string(),
            json!(self.group_sessions_per_user),
        );
        out.insert(
            "thread_sessions_per_user".to_string(),
            json!(self.thread_sessions_per_user),
        );
        out.insert(
            "max_concurrent_sessions".to_string(),
            match self.max_concurrent_sessions {
                Some(n) => json!(n),
                None => Value::Null,
            },
        );
        out.insert(
            "multiplex_profiles".to_string(),
            json!(self.multiplex_profiles),
        );
        out.insert(
            "multiplex_profile_allowlist".to_string(),
            match &self.multiplex_profile_allowlist {
                Some(list) => json!(list),
                None => Value::Null,
            },
        );
        out.insert(
            "room_link_url".to_string(),
            match &self.room_link_url {
                Some(s) => json!(s),
                None => Value::Null,
            },
        );
        out.insert(
            "systemd_watchdog_seconds".to_string(),
            json!(self.systemd_watchdog_seconds),
        );
        out.insert("loop_watchdog".to_string(), json!(self.loop_watchdog));
        out.insert(
            "loop_watchdog_probe_interval_s".to_string(),
            json!(self.loop_watchdog_probe_interval_s),
        );
        out.insert(
            "loop_watchdog_probe_timeout_s".to_string(),
            json!(self.loop_watchdog_probe_timeout_s),
        );
        out.insert(
            "loop_watchdog_max_strikes".to_string(),
            json!(self.loop_watchdog_max_strikes),
        );
        out.insert(
            "unauthorized_dm_behavior".to_string(),
            json!(self.unauthorized_dm_behavior),
        );
        out.insert("streaming".to_string(), self.streaming.to_dict());
        out.insert(
            "session_store_max_age_days".to_string(),
            json!(self.session_store_max_age_days),
        );
        out.insert(
            "profile_routes".to_string(),
            Value::Array(
                self.profile_routes
                    .iter()
                    .map(profile_route_to_dict)
                    .collect(),
            ),
        );

        Value::Object(out)
    }

    /// Port of `GatewayConfig.from_dict`.
    ///
    /// Follows the Python control flow closely: coerce the input to a dict, pull
    /// the nested `gateway` sub-map, resolve every field (honoring the
    /// key-or-nested precedence and the two null semantics), build the struct,
    /// then run `post_init` last. `_env_multiplex_profiles_override` and
    /// `parse_profile_routes` are reused from their ported homes.
    pub fn from_dict(value: &Value) -> Self {
        let data = coerce_dict(value);

        // Nested `gateway` sub-map (non-dict -> {}), source of the fallback layer.
        let nested: Map<String, Value> = match data.get("gateway") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };

        // platforms: skip non-dict entries and unknown platform keys.
        let mut platforms: HashMap<Platform, PlatformConfig> = HashMap::new();
        for (name, pdata) in coerce_dict(get(&data, "platforms")) {
            if !pdata.is_object() {
                continue;
            }
            if let Some(p) = parse_platform_key(&name) {
                platforms.insert(p, PlatformConfig::from_dict(&pdata));
            }
        }

        // reset_by_type: string keys kept as-is.
        let mut reset_by_type: BTreeMap<String, SessionResetPolicy> = BTreeMap::new();
        for (tname, pol) in coerce_dict(get(&data, "reset_by_type")) {
            reset_by_type.insert(tname, SessionResetPolicy::from_dict(&pol));
        }

        // reset_by_platform: unknown platform keys skipped.
        let mut reset_by_platform: HashMap<Platform, SessionResetPolicy> = HashMap::new();
        for (pname, pol) in coerce_dict(get(&data, "reset_by_platform")) {
            if let Some(p) = parse_platform_key(&pname) {
                reset_by_platform.insert(p, SessionResetPolicy::from_dict(&pol));
            }
        }

        let default_reset_policy = if data.contains_key("default_reset_policy") {
            SessionResetPolicy::from_dict(&data["default_reset_policy"])
        } else {
            SessionResetPolicy::default()
        };

        // sessions_dir: present key wins (stringified), else hermes_home()/sessions.
        let sessions_dir = if data.contains_key("sessions_dir") {
            PathBuf::from(py_str(&data["sessions_dir"]))
        } else {
            hermes_home().join("sessions")
        };

        // quick_commands: present-and-dict wins, else {}.
        let quick_commands: Map<String, Value> = match data.get("quick_commands") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };

        // stt_enabled: top-level, else the nested stt.enabled, else default True.
        let stt_enabled_val: Value = match data.get("stt_enabled") {
            Some(v) if !v.is_null() => v.clone(),
            _ => match data.get("stt") {
                Some(Value::Object(m)) => m.get("enabled").cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            },
        };
        let stt_echo_val: Value = match data.get("stt_echo_transcripts") {
            Some(v) if !v.is_null() => v.clone(),
            _ => match data.get("stt") {
                Some(Value::Object(m)) => m.get("echo_transcripts").cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            },
        };

        // multiplex_profiles: top-level (is-not-None) else nested, then env wins.
        let mut multiplex_raw: Value = match data.get("multiplex_profiles") {
            Some(v) if !v.is_null() => v.clone(),
            _ => nested
                .get("multiplex_profiles")
                .cloned()
                .unwrap_or(Value::Null),
        };
        if let Some(env_flag) = env_multiplex_profiles_override() {
            multiplex_raw = json!(env_flag);
        }
        let multiplex_profiles = coerce_bool(&multiplex_raw, false);

        // multiplex_profile_allowlist raw value (presence-based), normalized in post_init.
        let allowlist_raw: Value = if data.contains_key("multiplex_profile_allowlist") {
            data.get("multiplex_profile_allowlist")
                .cloned()
                .unwrap_or(Value::Null)
        } else {
            nested
                .get("multiplex_profile_allowlist")
                .cloned()
                .unwrap_or(Value::Null)
        };

        // room_link_url: kept only when it is a string.
        let room_link_url = match data.get("room_link_url") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        // systemd_watchdog_seconds: presence-based key-or-nested, coerced (re-coerced in post_init).
        let systemd_watchdog_seconds = coerce_systemd_watchdog_seconds(key_or_nested(
            &data,
            &nested,
            "systemd_watchdog_seconds",
        ));

        let loop_watchdog = coerce_bool(key_or_nested(&data, &nested, "loop_watchdog"), true);

        let mut loop_watchdog_probe_interval_s = coerce_float(
            key_or_nested(&data, &nested, "loop_watchdog_probe_interval_s"),
            DEFAULT_LOOP_WATCHDOG_INTERVAL_S,
        );
        let mut loop_watchdog_probe_timeout_s = coerce_float(
            key_or_nested(&data, &nested, "loop_watchdog_probe_timeout_s"),
            DEFAULT_LOOP_WATCHDOG_TIMEOUT_S,
        );
        let mut loop_watchdog_max_strikes = coerce_int(
            key_or_nested(&data, &nested, "loop_watchdog_max_strikes"),
            DEFAULT_LOOP_WATCHDOG_MAX_STRIKES,
        );
        if !loop_watchdog_probe_interval_s.is_finite()
            || !(1.0..=3600.0).contains(&loop_watchdog_probe_interval_s)
        {
            loop_watchdog_probe_interval_s = DEFAULT_LOOP_WATCHDOG_INTERVAL_S;
        }
        if !loop_watchdog_probe_timeout_s.is_finite()
            || !(1.0..=600.0).contains(&loop_watchdog_probe_timeout_s)
        {
            loop_watchdog_probe_timeout_s = DEFAULT_LOOP_WATCHDOG_TIMEOUT_S;
        }
        if !(1..=1000).contains(&loop_watchdog_max_strikes) {
            loop_watchdog_max_strikes = DEFAULT_LOOP_WATCHDOG_MAX_STRIKES;
        }

        let max_concurrent_sessions =
            coerce_optional_positive_int(key_or_nested(&data, &nested, "max_concurrent_sessions"));

        let unauthorized_dm_behavior =
            normalize_unauthorized_dm_behavior(get(&data, "unauthorized_dm_behavior"), "pair");

        // session_store_max_age_days: int(data.get(...,90)) then max(0), default 90 on failure.
        let raw_age = if data.contains_key("session_store_max_age_days") {
            data["session_store_max_age_days"].clone()
        } else {
            json!(90)
        };
        let session_store_max_age_days = coerce_int(&raw_age, 90).max(0);

        let profile_routes = parse_profile_routes(data.get("profile_routes"));

        let mut cfg = GatewayConfig {
            platforms,
            default_reset_policy,
            reset_by_type,
            reset_by_platform,
            reset_triggers: if data.contains_key("reset_triggers") {
                data["reset_triggers"].clone()
            } else {
                json!(["/new", "/reset"])
            },
            quick_commands,
            sessions_dir,
            write_sessions_json: coerce_bool(get(&data, "write_sessions_json"), true),
            always_log_local: coerce_bool(get(&data, "always_log_local"), true),
            filter_silence_narration: coerce_bool(get(&data, "filter_silence_narration"), true),
            stt_enabled: coerce_bool(&stt_enabled_val, true),
            stt_echo_transcripts: coerce_bool(&stt_echo_val, true),
            group_sessions_per_user: coerce_bool(get(&data, "group_sessions_per_user"), true),
            thread_sessions_per_user: coerce_bool(get(&data, "thread_sessions_per_user"), false),
            max_concurrent_sessions,
            multiplex_profiles,
            // Placeholder; post_init fills this from allowlist_raw.
            multiplex_profile_allowlist: None,
            room_link_url,
            systemd_watchdog_seconds,
            loop_watchdog,
            loop_watchdog_probe_interval_s,
            loop_watchdog_probe_timeout_s,
            loop_watchdog_max_strikes,
            unauthorized_dm_behavior,
            streaming: StreamingConfig::from_dict(get(&data, "streaming")),
            session_store_max_age_days,
            profile_routes,
        };
        cfg.post_init(&allowlist_raw);
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- _has_usable_api_server_key ----------------------------------------

    #[test]
    fn has_usable_api_server_key_matches_python() {
        // Locked against gateway.config._has_usable_api_server_key.
        assert!(!has_usable_api_server_key(&Value::Null));
        assert!(!has_usable_api_server_key(&json!("")));
        assert!(!has_usable_api_server_key(&json!(0)));
        // Non-string truthy: passes the `not key` guard, fails isinstance(str).
        assert!(!has_usable_api_server_key(&json!(12345)));
        assert!(!has_usable_api_server_key(&json!(true)));
        assert!(!has_usable_api_server_key(&json!([])));
        assert!(!has_usable_api_server_key(&json!({})));
        // Too short after strip.
        assert!(!has_usable_api_server_key(&json!("short")));
        assert!(!has_usable_api_server_key(&json!("   ")));
        // Placeholders (case-folded, stripped).
        assert!(!has_usable_api_server_key(&json!("placeholder")));
        assert!(!has_usable_api_server_key(&json!("  CHANGEME  ")));
        assert!(!has_usable_api_server_key(&json!("Dummy")));
        assert!(!has_usable_api_server_key(&json!("  ***  ")));
        // Usable: >= 16 chars, not a placeholder.
        assert!(has_usable_api_server_key(&json!(
            "supersecretkey-1234567890"
        )));
        assert!(has_usable_api_server_key(&json!("xxxxxxxxxxxxxxxx"))); // exactly 16
    }

    // --- Golden from_dict -> to_dict round-trips ---------------------------
    //
    // Each golden string is the verbatim output of, from the repo root:
    //   python3 -c "import sys; sys.path.insert(0,'.'); \
    //     from gateway.config import GatewayConfig; import json; \
    //     print(json.dumps(GatewayConfig.from_dict({...}).to_dict(), \
    //                      sort_keys=True, default=str))"
    // with HERMES_HOME=/tmp/hgold and GATEWAY_MULTIPLEX_PROFILES unset.
    //
    // Comparison normalization: we parse the Python JSON into a serde_json::Value
    // and compare Values, not strings. That absorbs (a) Python's ascii-escaping
    // (the streaming cursor ▉ vs serde's raw UTF-8 ▉), (b) object key order,
    // and (c) int-vs-float spelling. For the default-sessions goldens we inject
    // this machine's hermes_home()/sessions into the expected Value so the test
    // does not depend on HERMES_HOME being set.

    fn assert_golden(input: Value, golden: &str, default_sessions: bool) {
        // The env override is consulted by from_dict; nothing else sets it, so a
        // one-shot remove keeps the round-trip deterministic.
        std::env::remove_var("GATEWAY_MULTIPLEX_PROFILES");
        let got = GatewayConfig::from_dict(&input).to_dict();
        let mut expected: Value = serde_json::from_str(golden).expect("golden parses");
        if default_sessions {
            expected["sessions_dir"] =
                json!(hermes_home().join("sessions").to_string_lossy().to_string());
        }
        assert_eq!(got, expected, "input was {input}");
    }

    #[test]
    fn golden_empty_all_defaults() {
        assert_golden(
            json!({}),
            r#"{"always_log_local": true, "default_reset_policy": {"at_hour": 4, "bg_process_max_age_hours": 24, "idle_minutes": 1440, "mode": "none", "notify": true, "notify_exclude_platforms": ["api_server", "webhook"]}, "filter_silence_narration": true, "group_sessions_per_user": true, "loop_watchdog": true, "loop_watchdog_max_strikes": 3, "loop_watchdog_probe_interval_s": 30.0, "loop_watchdog_probe_timeout_s": 10.0, "max_concurrent_sessions": null, "multiplex_profile_allowlist": null, "multiplex_profiles": false, "platforms": {}, "profile_routes": [], "quick_commands": {}, "reset_by_platform": {}, "reset_by_type": {}, "reset_triggers": ["/new", "/reset"], "room_link_url": null, "session_store_max_age_days": 90, "sessions_dir": "/tmp/hgold/sessions", "streaming": {"buffer_threshold": 24, "cursor": " ▉", "edit_interval": 0.8, "enabled": false, "fresh_final_after_seconds": 0.0, "transport": "auto"}, "stt_echo_transcripts": true, "stt_enabled": true, "systemd_watchdog_seconds": 0, "thread_sessions_per_user": false, "unauthorized_dm_behavior": "pair", "write_sessions_json": true}"#,
            true,
        );
    }

    #[test]
    fn golden_populated_platforms_and_policies() {
        // 'Telegram' capitalized -> normalized to telegram; 'made_up_platform'
        // is not a built-in value -> skipped; reset_by_platform 'bogus' skipped.
        assert_golden(
            json!({
                "platforms": {
                    "Telegram": {"enabled": true, "token": "tok123",
                                 "home_channel": {"platform": "telegram", "chat_id": "111", "name": "Home"}},
                    "api_server": {"enabled": true, "extra": {"key": "supersecretkey-1234567890"}},
                    "made_up_platform": {"enabled": true}
                },
                "default_reset_policy": {"mode": "idle", "at_hour": 5, "idle_minutes": 60},
                "reset_by_type": {"dm": {"mode": "daily"}, "group": {"mode": "none"}},
                "reset_by_platform": {"telegram": {"mode": "idle"}, "bogus": {"mode": "x"}},
                "streaming": {"enabled": true, "transport": "edit", "buffer_threshold": 40},
                "reset_triggers": ["/new", "/reset", "/clear"],
                "quick_commands": {"/ping": {"reply": "pong"}}
            }),
            r#"{"always_log_local": true, "default_reset_policy": {"at_hour": 5, "bg_process_max_age_hours": 24, "idle_minutes": 60, "mode": "idle", "notify": true, "notify_exclude_platforms": ["api_server", "webhook"]}, "filter_silence_narration": true, "group_sessions_per_user": true, "loop_watchdog": true, "loop_watchdog_max_strikes": 3, "loop_watchdog_probe_interval_s": 30.0, "loop_watchdog_probe_timeout_s": 10.0, "max_concurrent_sessions": null, "multiplex_profile_allowlist": null, "multiplex_profiles": false, "platforms": {"api_server": {"enabled": true, "extra": {"key": "supersecretkey-1234567890"}, "gateway_restart_notification": true, "reply_to_mode": "first", "typing_indicator": true}, "telegram": {"enabled": true, "extra": {}, "gateway_restart_notification": true, "home_channel": {"chat_id": "111", "name": "Home", "platform": "telegram"}, "reply_to_mode": "first", "token": "tok123", "typing_indicator": true}}, "profile_routes": [], "quick_commands": {"/ping": {"reply": "pong"}}, "reset_by_platform": {"telegram": {"at_hour": 4, "bg_process_max_age_hours": 24, "idle_minutes": 1440, "mode": "idle", "notify": true, "notify_exclude_platforms": ["api_server", "webhook"]}}, "reset_by_type": {"dm": {"at_hour": 4, "bg_process_max_age_hours": 24, "idle_minutes": 1440, "mode": "daily", "notify": true, "notify_exclude_platforms": ["api_server", "webhook"]}, "group": {"at_hour": 4, "bg_process_max_age_hours": 24, "idle_minutes": 1440, "mode": "none", "notify": true, "notify_exclude_platforms": ["api_server", "webhook"]}}, "reset_triggers": ["/new", "/reset", "/clear"], "room_link_url": null, "session_store_max_age_days": 90, "sessions_dir": "/tmp/hgold/sessions", "streaming": {"buffer_threshold": 40, "cursor": " ▉", "edit_interval": 0.8, "enabled": true, "fresh_final_after_seconds": 0.0, "transport": "edit"}, "stt_echo_transcripts": true, "stt_enabled": true, "systemd_watchdog_seconds": 0, "thread_sessions_per_user": false, "unauthorized_dm_behavior": "pair", "write_sessions_json": true}"#,
            true,
        );
    }

    #[test]
    fn golden_multiplex_watchdog_api_server_fields() {
        assert_golden(
            json!({
                "multiplex_profiles": true,
                "multiplex_profile_allowlist": ["Alpha", "beta", "beta", "default", "hermes"],
                "room_link_url": "https://example.com/link",
                "systemd_watchdog_seconds": 45,
                "loop_watchdog": false,
                "loop_watchdog_probe_interval_s": 15.5,
                "loop_watchdog_probe_timeout_s": 8.0,
                "loop_watchdog_max_strikes": 5,
                "max_concurrent_sessions": 12,
                "unauthorized_dm_behavior": " IGNORE ",
                "session_store_max_age_days": 30
            }),
            r#"{"always_log_local": true, "default_reset_policy": {"at_hour": 4, "bg_process_max_age_hours": 24, "idle_minutes": 1440, "mode": "none", "notify": true, "notify_exclude_platforms": ["api_server", "webhook"]}, "filter_silence_narration": true, "group_sessions_per_user": true, "loop_watchdog": false, "loop_watchdog_max_strikes": 5, "loop_watchdog_probe_interval_s": 15.5, "loop_watchdog_probe_timeout_s": 8.0, "max_concurrent_sessions": 12, "multiplex_profile_allowlist": ["alpha", "beta"], "multiplex_profiles": true, "platforms": {}, "profile_routes": [], "quick_commands": {}, "reset_by_platform": {}, "reset_by_type": {}, "reset_triggers": ["/new", "/reset"], "room_link_url": "https://example.com/link", "session_store_max_age_days": 30, "sessions_dir": "/tmp/hgold/sessions", "streaming": {"buffer_threshold": 24, "cursor": " ▉", "edit_interval": 0.8, "enabled": false, "fresh_final_after_seconds": 0.0, "transport": "auto"}, "stt_echo_transcripts": true, "stt_enabled": true, "systemd_watchdog_seconds": 45, "thread_sessions_per_user": false, "unauthorized_dm_behavior": "ignore", "write_sessions_json": true}"#,
            true,
        );
    }

    #[test]
    fn golden_post_init_normalizations_and_clamps() {
        // allowlist non-list -> []; systemd > max -> 0; interval < 1 -> 30.0;
        // timeout > 600 -> 10.0; strikes < 1 -> 3; max_concurrent <=0 -> null;
        // age < 0 -> 0; unauthorized garbage -> pair; write_sessions_json off;
        // nested stt disables both.
        assert_golden(
            json!({
                "multiplex_profile_allowlist": "not-a-list",
                "systemd_watchdog_seconds": 99999999999i64,
                "loop_watchdog_probe_interval_s": 0.5,
                "loop_watchdog_probe_timeout_s": 9000,
                "loop_watchdog_max_strikes": 0,
                "max_concurrent_sessions": -5,
                "session_store_max_age_days": -10,
                "unauthorized_dm_behavior": "garbage",
                "write_sessions_json": "off",
                "stt": {"enabled": false, "echo_transcripts": false}
            }),
            r#"{"always_log_local": true, "default_reset_policy": {"at_hour": 4, "bg_process_max_age_hours": 24, "idle_minutes": 1440, "mode": "none", "notify": true, "notify_exclude_platforms": ["api_server", "webhook"]}, "filter_silence_narration": true, "group_sessions_per_user": true, "loop_watchdog": true, "loop_watchdog_max_strikes": 3, "loop_watchdog_probe_interval_s": 30.0, "loop_watchdog_probe_timeout_s": 10.0, "max_concurrent_sessions": null, "multiplex_profile_allowlist": [], "multiplex_profiles": false, "platforms": {}, "profile_routes": [], "quick_commands": {}, "reset_by_platform": {}, "reset_by_type": {}, "reset_triggers": ["/new", "/reset"], "room_link_url": null, "session_store_max_age_days": 0, "sessions_dir": "/tmp/hgold/sessions", "streaming": {"buffer_threshold": 24, "cursor": " ▉", "edit_interval": 0.8, "enabled": false, "fresh_final_after_seconds": 0.0, "transport": "auto"}, "stt_echo_transcripts": false, "stt_enabled": false, "systemd_watchdog_seconds": 0, "thread_sessions_per_user": false, "unauthorized_dm_behavior": "pair", "write_sessions_json": false}"#,
            true,
        );
    }

    #[test]
    fn golden_nested_gateway_and_profile_routes() {
        // Nested gateway.* keys feed the fallback layer; profile_routes are parsed
        // and sorted most-specific-first (thread spec 12 before guild spec 2), with
        // the profile name normalized (MyProfile -> myprofile). Explicit sessions_dir.
        assert_golden(
            json!({
                "sessions_dir": "/custom/sessions",
                "gateway": {
                    "loop_watchdog": false,
                    "max_concurrent_sessions": 7,
                    "multiplex_profile_allowlist": ["one", "two"],
                    "systemd_watchdog_seconds": 20,
                    "loop_watchdog_probe_interval_s": 12.0,
                    "multiplex_profiles": true
                },
                "profile_routes": [
                    {"name": "guild", "platform": "discord", "guild_id": 111, "profile": "server"},
                    {"name": "thread", "platform": "discord", "chat_id": 222, "thread_id": 333, "profile": "MyProfile"}
                ]
            }),
            r#"{"always_log_local": true, "default_reset_policy": {"at_hour": 4, "bg_process_max_age_hours": 24, "idle_minutes": 1440, "mode": "none", "notify": true, "notify_exclude_platforms": ["api_server", "webhook"]}, "filter_silence_narration": true, "group_sessions_per_user": true, "loop_watchdog": false, "loop_watchdog_max_strikes": 3, "loop_watchdog_probe_interval_s": 12.0, "loop_watchdog_probe_timeout_s": 10.0, "max_concurrent_sessions": 7, "multiplex_profile_allowlist": ["one", "two"], "multiplex_profiles": true, "platforms": {}, "profile_routes": [{"chat_id": "222", "enabled": true, "guild_id": null, "name": "thread", "platform": "discord", "profile": "myprofile", "thread_id": "333"}, {"chat_id": null, "enabled": true, "guild_id": "111", "name": "guild", "platform": "discord", "profile": "server", "thread_id": null}], "quick_commands": {}, "reset_by_platform": {}, "reset_by_type": {}, "reset_triggers": ["/new", "/reset"], "room_link_url": null, "session_store_max_age_days": 90, "sessions_dir": "/custom/sessions", "streaming": {"buffer_threshold": 24, "cursor": " ▉", "edit_interval": 0.8, "enabled": false, "fresh_final_after_seconds": 0.0, "transport": "auto"}, "stt_echo_transcripts": true, "stt_enabled": true, "systemd_watchdog_seconds": 20, "thread_sessions_per_user": false, "unauthorized_dm_behavior": "pair", "write_sessions_json": true}"#,
            false,
        );
    }

    // --- Default matches the empty-dict from_dict --------------------------

    #[test]
    fn default_matches_empty_from_dict() {
        std::env::remove_var("GATEWAY_MULTIPLEX_PROFILES");
        // Both read hermes_home() for sessions_dir, so no env pinning needed.
        assert_eq!(
            GatewayConfig::default().to_dict(),
            GatewayConfig::from_dict(&json!({})).to_dict()
        );
    }

    // --- Unknown-platform-key handling -------------------------------------

    #[test]
    fn unknown_platform_keys_are_skipped() {
        std::env::remove_var("GATEWAY_MULTIPLEX_PROFILES");
        let cfg = GatewayConfig::from_dict(&json!({
            "platforms": {
                "telegram": {"enabled": true},
                "made_up": {"enabled": true},
                "": {"enabled": true},
                "not_a_dict_value": 5
            },
            "reset_by_platform": {"discord": {"mode": "idle"}, "bogus": {"mode": "x"}}
        }));
        assert!(cfg.platforms.contains_key(&Platform::Telegram));
        assert_eq!(cfg.platforms.len(), 1);
        assert!(cfg.reset_by_platform.contains_key(&Platform::Discord));
        assert_eq!(cfg.reset_by_platform.len(), 1);
    }
}
