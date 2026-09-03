//! Per-platform display and verbosity configuration resolver.
//!
// Public API is ahead of its callers while the gateway pipeline is ported.
#![allow(dead_code)]
//!
//! Port of `gateway/display_config.py`. Provides `resolve_display_setting` as
//! the single entry point for reading display settings with platform-specific
//! overrides and sensible defaults.
//!
//! Resolution order (first non-null wins):
//! 1. `display.platforms.<platform>.<key>` (explicit per-platform user override)
//! 2. `display.<key>` (global user setting, skipping `streaming`)
//! 3. `_PLATFORM_DEFAULTS[<platform>][<key>]` (built-in platform default)
//! 4. `_GLOBAL_DEFAULTS[<key>]` (built-in global default)
//! 5. `fallback` value passed by caller

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Canonical set of per-platform overrideable display setting keys.
pub const OVERRIDEABLE_KEYS: &[&str] = &[
    "tool_progress",
    "tool_progress_grouping",
    "show_reasoning",
    "reasoning_style",
    "tool_preview_length",
    "streaming",
    "interim_assistant_messages",
    "long_running_notifications",
    "busy_ack_detail",
    "busy_steer_ack_enabled",
    "cleanup_progress",
    "live_status",
];

/// Returns true if `key` is in the canonical list of overrideable display settings.
pub fn is_overrideable_key(key: &str) -> bool {
    OVERRIDEABLE_KEYS.contains(&key)
}

/// Tool progress output verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolProgress {
    Off,
    New,
    All,
    Verbose,
    Log,
}

impl ToolProgress {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::New => "new",
            Self::All => "all",
            Self::Verbose => "verbose",
            Self::Log => "log",
        }
    }

    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::Bool(false) => Self::Off,
            Value::Bool(true) => Self::All,
            Value::String(s) => match s.trim().to_lowercase().as_str() {
                "false" | "0" | "no" | "off" => Self::Off,
                "new" => Self::New,
                "verbose" => Self::Verbose,
                "log" => Self::Log,
                _ => Self::All,
            },
            Value::Number(n) => match n.to_string().trim().to_lowercase().as_str() {
                "0" => Self::Off,
                _ => Self::All,
            },
            _ => Self::All,
        }
    }
}

impl fmt::Display for ToolProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ToolProgress {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_lowercase().as_str() {
            "false" | "0" | "no" | "off" => Self::Off,
            "new" => Self::New,
            "verbose" => Self::Verbose,
            "log" => Self::Log,
            _ => Self::All,
        })
    }
}

/// Tool progress bubble grouping mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolProgressGrouping {
    #[default]
    Accumulate,
    Separate,
}

impl ToolProgressGrouping {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accumulate => "accumulate",
            Self::Separate => "separate",
        }
    }

    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::String(s) => match s.trim().to_lowercase().as_str() {
                "separate" => Self::Separate,
                _ => Self::Accumulate,
            },
            _ => Self::Accumulate,
        }
    }
}

impl fmt::Display for ToolProgressGrouping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ToolProgressGrouping {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_lowercase().as_str() {
            "separate" => Self::Separate,
            _ => Self::Accumulate,
        })
    }
}

/// Rendering style for reasoning summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningStyle {
    #[default]
    Code,
    Blockquote,
    Subtext,
}

impl ReasoningStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Blockquote => "blockquote",
            Self::Subtext => "subtext",
        }
    }

    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::String(s) => match s.trim().to_lowercase().as_str() {
                "blockquote" => Self::Blockquote,
                "subtext" => Self::Subtext,
                _ => Self::Code,
            },
            _ => Self::Code,
        }
    }
}

impl fmt::Display for ReasoningStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningStyle {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_lowercase().as_str() {
            "blockquote" => Self::Blockquote,
            "subtext" => Self::Subtext,
            _ => Self::Code,
        })
    }
}

/// Live working status rendering mode in typing indicators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LiveStatus {
    #[default]
    Full,
    Verb,
    Off,
}

impl LiveStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Verb => "verb",
            Self::Off => "off",
        }
    }

    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::Bool(true) => Self::Full,
            Value::Bool(false) => Self::Off,
            Value::String(s) => match s.trim().to_lowercase().as_str() {
                "true" | "1" | "yes" | "on" | "all" => Self::Full,
                "false" | "0" | "no" | "off" => Self::Off,
                "verb" => Self::Verb,
                _ => Self::Full,
            },
            Value::Number(n) => match n.to_string().trim().to_lowercase().as_str() {
                "0" => Self::Off,
                _ => Self::Full,
            },
            _ => Self::Full,
        }
    }
}

impl fmt::Display for LiveStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LiveStatus {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_lowercase().as_str() {
            "false" | "0" | "no" | "off" => Self::Off,
            "verb" => Self::Verb,
            _ => Self::Full,
        })
    }
}

/// Long running notification verbosity mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LongRunningNotifications {
    Off,
    On,
    Generic,
}

impl LongRunningNotifications {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Generic => "generic",
        }
    }

    pub fn from_value(value: &Value) -> Self {
        match value {
            Value::String(s) => {
                let trimmed = s.trim().to_lowercase();
                if trimmed == "generic" {
                    Self::Generic
                } else if matches!(
                    trimmed.as_str(),
                    "true" | "1" | "yes" | "on" | "raw" | "verbose"
                ) {
                    Self::On
                } else {
                    Self::Off
                }
            }
            _ => {
                if is_truthy(value) {
                    Self::On
                } else {
                    Self::Off
                }
            }
        }
    }
}

impl fmt::Display for LongRunningNotifications {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Platform capability tiers for display defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlatformCapabilityTier {
    /// Tier 1: Supports message editing, personal and team channels.
    High,
    /// Tier 2: Supports editing, often customer and workspace facing.
    Medium,
    /// Tier 3: No edit support, each progress message is permanent.
    Low,
    /// Tier 4: Batch or non-interactive delivery.
    Minimal,
}

/// Helper for Python truthiness check on JSON values.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Normalise YAML quirks and type discrepancies for display settings.
pub fn normalise(setting: &str, value: &Value) -> Value {
    match setting {
        "tool_progress" => {
            if let Some(b) = value.as_bool() {
                return Value::String((if b { "all" } else { "off" }).to_string());
            }
            let s = match value {
                Value::String(s) => s.trim().to_lowercase(),
                Value::Number(n) => n.to_string().trim().to_lowercase(),
                _ => return Value::String("all".to_string()),
            };
            match s.as_str() {
                "false" | "0" | "no" => Value::String("off".to_string()),
                "true" | "1" | "yes" | "on" => Value::String("all".to_string()),
                "off" | "new" | "all" | "verbose" | "log" => Value::String(s),
                _ => Value::String("all".to_string()),
            }
        }
        "show_reasoning"
        | "streaming"
        | "interim_assistant_messages"
        | "long_running_notifications"
        | "busy_ack_detail"
        | "busy_steer_ack_enabled"
        | "thinking_progress" => {
            if let Value::String(s) = value {
                let trimmed = s.trim().to_lowercase();
                if setting == "long_running_notifications" && trimmed == "generic" {
                    return Value::String("generic".to_string());
                }
                let is_true = matches!(
                    trimmed.as_str(),
                    "true" | "1" | "yes" | "on" | "raw" | "verbose"
                );
                return Value::Bool(is_true);
            }
            Value::Bool(is_truthy(value))
        }
        "cleanup_progress" => {
            if let Value::String(s) = value {
                let trimmed = s.trim().to_lowercase();
                let is_true = matches!(trimmed.as_str(), "true" | "1" | "yes" | "on");
                return Value::Bool(is_true);
            }
            Value::Bool(is_truthy(value))
        }
        "live_status" => {
            if let Some(b) = value.as_bool() {
                return Value::String((if b { "full" } else { "off" }).to_string());
            }
            let s = match value {
                Value::String(s) => s.trim().to_lowercase(),
                Value::Number(n) => n.to_string().trim().to_lowercase(),
                _ => return Value::String("full".to_string()),
            };
            match s.as_str() {
                "true" | "1" | "yes" | "on" | "all" => Value::String("full".to_string()),
                "false" | "0" | "no" => Value::String("off".to_string()),
                "full" | "verb" | "off" => Value::String(s),
                _ => Value::String("full".to_string()),
            }
        }
        "tool_progress_grouping" => {
            let s = match value {
                Value::String(s) => s.trim().to_lowercase(),
                _ => String::new(),
            };
            match s.as_str() {
                "accumulate" | "separate" => Value::String(s),
                _ => Value::String("accumulate".to_string()),
            }
        }
        "reasoning_style" => {
            let s = match value {
                Value::String(s) => s.trim().to_lowercase(),
                _ => String::new(),
            };
            match s.as_str() {
                "code" | "blockquote" | "subtext" => Value::String(s),
                _ => Value::String("code".to_string()),
            }
        }
        "tool_preview_length" => match value {
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Number(i.into())
                } else if let Some(u) = n.as_u64() {
                    Value::Number(u.into())
                } else if let Some(f) = n.as_f64() {
                    Value::Number((f as i64).into())
                } else {
                    Value::Number(0.into())
                }
            }
            Value::String(s) => match s.trim().parse::<i64>() {
                Ok(i) => Value::Number(i.into()),
                Err(_) => Value::Number(0.into()),
            },
            Value::Bool(b) => Value::Number((if *b { 1 } else { 0 }).into()),
            _ => Value::Number(0.into()),
        },
        _ => value.clone(),
    }
}

/// Platform capability tier classification for built-in platforms.
pub fn platform_capability_tier(platform: &str) -> Option<PlatformCapabilityTier> {
    match platform {
        "telegram" | "discord" | "api_server" => Some(PlatformCapabilityTier::High),
        "slack" | "mattermost" | "matrix" | "feishu" | "buzz" | "whatsapp" => {
            Some(PlatformCapabilityTier::Medium)
        }
        "signal" | "whatsapp_cloud" | "photon" | "bluebubbles" | "weixin" | "wecom"
        | "wecom_callback" | "dingtalk" => Some(PlatformCapabilityTier::Low),
        "email" | "sms" | "webhook" | "homeassistant" => Some(PlatformCapabilityTier::Minimal),
        _ => None,
    }
}

/// Built-in per-platform default for a given setting.
pub fn platform_default_setting(platform: &str, setting: &str) -> Option<Value> {
    // 1. Explicit per-platform overrides from _PLATFORM_DEFAULTS.
    match (platform, setting) {
        ("telegram", "tool_progress") => return Some(json!("off")),
        ("telegram", "busy_ack_detail") => return Some(json!(false)),
        ("discord", "reasoning_style") => return Some(json!("subtext")),
        ("api_server", "tool_preview_length") => return Some(json!(0)),
        ("slack", "tool_progress") => return Some(json!("off")),
        ("slack", "long_running_notifications") => return Some(json!(false)),
        ("slack", "busy_ack_detail") => return Some(json!(false)),
        ("wecom", "streaming") => return Some(json!(true)),
        _ => {}
    }

    // 2. Base tier defaults.
    let tier = platform_capability_tier(platform)?;
    match tier {
        PlatformCapabilityTier::High => match setting {
            "tool_progress" => Some(json!("all")),
            "show_reasoning" => Some(json!(false)),
            "tool_preview_length" => Some(json!(40)),
            "streaming" => None,
            "interim_assistant_messages" => Some(json!(true)),
            "long_running_notifications" => Some(json!(true)),
            "busy_ack_detail" => Some(json!(true)),
            _ => None,
        },
        PlatformCapabilityTier::Medium => match setting {
            "tool_progress" => Some(json!("new")),
            "show_reasoning" => Some(json!(false)),
            "tool_preview_length" => Some(json!(40)),
            "streaming" => None,
            "interim_assistant_messages" => Some(json!(true)),
            "long_running_notifications" => Some(json!(true)),
            "busy_ack_detail" => Some(json!(true)),
            _ => None,
        },
        PlatformCapabilityTier::Low => match setting {
            "tool_progress" => Some(json!("off")),
            "show_reasoning" => Some(json!(false)),
            "tool_preview_length" => Some(json!(40)),
            "streaming" => Some(json!(false)),
            "interim_assistant_messages" => Some(json!(false)),
            "long_running_notifications" => Some(json!(false)),
            "busy_ack_detail" => Some(json!(false)),
            _ => None,
        },
        PlatformCapabilityTier::Minimal => match setting {
            "tool_progress" => Some(json!("off")),
            "show_reasoning" => Some(json!(false)),
            "tool_preview_length" => Some(json!(0)),
            "streaming" => Some(json!(false)),
            "interim_assistant_messages" => Some(json!(false)),
            "long_running_notifications" => Some(json!(false)),
            "busy_ack_detail" => Some(json!(false)),
            _ => None,
        },
    }
}

/// Built-in global default for a given display setting.
pub fn global_default_setting(setting: &str) -> Option<Value> {
    match setting {
        "tool_progress" => Some(json!("all")),
        "tool_progress_grouping" => Some(json!("accumulate")),
        "show_reasoning" => Some(json!(false)),
        "reasoning_style" => Some(json!("code")),
        "tool_preview_length" => Some(json!(0)),
        "streaming" => None,
        "interim_assistant_messages" => Some(json!(true)),
        "long_running_notifications" => Some(json!(true)),
        "busy_ack_detail" => Some(json!(true)),
        "busy_steer_ack_enabled" => Some(json!(true)),
        "cleanup_progress" => Some(json!(false)),
        "live_status" => Some(json!("full")),
        _ => None,
    }
}

/// Resolve a display setting with per-platform override support.
///
/// Parameters:
/// - `user_config`: The full parsed configuration JSON object.
/// - `platform_key`: Platform config key (e.g. `"telegram"`, `"slack"`).
/// - `setting`: Display setting name (e.g. `"tool_progress"`, `"show_reasoning"`).
/// - `fallback`: Fallback value when the setting is not configured anywhere.
///
/// Resolution order:
/// 1. `display.platforms.<platform>.<key>` (explicit per-platform user override)
///    1b. `display.tool_progress_overrides.<platform>` (legacy tool_progress fallback)
/// 2. `display.<key>` (global user setting, skipped for `streaming`)
/// 3. Built-in platform default (`platform_default_setting`)
/// 4. Built-in global default (`global_default_setting`)
/// 5. `fallback` (or `Value::Null` if omitted)
pub fn resolve_display_setting(
    user_config: &Value,
    platform_key: &str,
    setting: &str,
    fallback: Option<Value>,
) -> Value {
    let display_cfg = user_config.get("display").and_then(|v| v.as_object());

    if let Some(display) = display_cfg {
        // 1. Explicit per-platform override: display.platforms.<platform>.<key>
        if let Some(platforms) = display.get("platforms").and_then(|v| v.as_object()) {
            if let Some(plat_overrides) = platforms.get(platform_key).and_then(|v| v.as_object()) {
                if let Some(val) = plat_overrides.get(setting) {
                    if !val.is_null() {
                        return normalise(setting, val);
                    }
                }
            }
        }

        // 1b. Backward compatibility: display.tool_progress_overrides.<platform>
        if setting == "tool_progress" {
            if let Some(legacy) = display
                .get("tool_progress_overrides")
                .and_then(|v| v.as_object())
            {
                if let Some(val) = legacy.get(platform_key) {
                    if !val.is_null() {
                        return normalise(setting, val);
                    }
                }
            }
        }

        // 2. Global user setting: display.<key>. Skip display.streaming because
        // that key controls CLI terminal streaming only; gateway streaming follows
        // top-level streaming or per-platform display overrides.
        if setting != "streaming" {
            if let Some(val) = display.get(setting) {
                if !val.is_null() {
                    return normalise(setting, val);
                }
            }
        }
    }

    // 3. Built-in platform default
    if let Some(val) = platform_default_setting(platform_key, setting) {
        return val;
    }

    // 4. Built-in global default
    if let Some(val) = global_default_setting(setting) {
        return val;
    }

    fallback.unwrap_or(Value::Null)
}

/// Resolved typed display configuration for a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedDisplayConfig {
    pub tool_progress: ToolProgress,
    pub tool_progress_grouping: ToolProgressGrouping,
    pub show_reasoning: bool,
    pub reasoning_style: ReasoningStyle,
    pub tool_preview_length: usize,
    pub streaming: Option<bool>,
    pub interim_assistant_messages: bool,
    pub long_running_notifications: LongRunningNotifications,
    pub busy_ack_detail: bool,
    pub busy_steer_ack_enabled: bool,
    pub cleanup_progress: bool,
    pub live_status: LiveStatus,
}

impl ResolvedDisplayConfig {
    /// Resolve all display settings for a platform from the user configuration.
    pub fn resolve(user_config: &Value, platform_key: &str) -> Self {
        let tp_val = resolve_display_setting(user_config, platform_key, "tool_progress", None);
        let tool_progress = ToolProgress::from_value(&tp_val);

        let tpg_val =
            resolve_display_setting(user_config, platform_key, "tool_progress_grouping", None);
        let tool_progress_grouping = ToolProgressGrouping::from_value(&tpg_val);

        let sr_val = resolve_display_setting(user_config, platform_key, "show_reasoning", None);
        let show_reasoning = sr_val.as_bool().unwrap_or(false);

        let rs_val = resolve_display_setting(user_config, platform_key, "reasoning_style", None);
        let reasoning_style = ReasoningStyle::from_value(&rs_val);

        let tpl_val =
            resolve_display_setting(user_config, platform_key, "tool_preview_length", None);
        let tool_preview_length = tpl_val.as_u64().map(|u| u as usize).unwrap_or(0);

        let stream_val = resolve_display_setting(user_config, platform_key, "streaming", None);
        let streaming = stream_val.as_bool();

        let iam_val = resolve_display_setting(
            user_config,
            platform_key,
            "interim_assistant_messages",
            None,
        );
        let interim_assistant_messages = iam_val.as_bool().unwrap_or(true);

        let lrn_val = resolve_display_setting(
            user_config,
            platform_key,
            "long_running_notifications",
            None,
        );
        let long_running_notifications = LongRunningNotifications::from_value(&lrn_val);

        let bad_val = resolve_display_setting(user_config, platform_key, "busy_ack_detail", None);
        let busy_ack_detail = bad_val.as_bool().unwrap_or(true);

        let bsae_val =
            resolve_display_setting(user_config, platform_key, "busy_steer_ack_enabled", None);
        let busy_steer_ack_enabled = bsae_val.as_bool().unwrap_or(true);

        let cp_val = resolve_display_setting(user_config, platform_key, "cleanup_progress", None);
        let cleanup_progress = cp_val.as_bool().unwrap_or(false);

        let ls_val = resolve_display_setting(user_config, platform_key, "live_status", None);
        let live_status = LiveStatus::from_value(&ls_val);

        Self {
            tool_progress,
            tool_progress_grouping,
            show_reasoning,
            reasoning_style,
            tool_preview_length,
            streaming,
            interim_assistant_messages,
            long_running_notifications,
            busy_ack_detail,
            busy_steer_ack_enabled,
            cleanup_progress,
            live_status,
        }
    }
}

/// Optional typed display configuration block as parsed from config.yaml.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DisplayConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_progress: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_progress_grouping: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_reasoning: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_style: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_preview_length: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interim_assistant_messages: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_running_notifications: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_ack_detail: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_steer_ack_enabled: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_progress: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_status: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<HashMap<String, HashMap<String, Value>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_progress_overrides: Option<HashMap<String, Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_platform_override_wins() {
        let config = json!({
            "display": {
                "tool_progress": "all",
                "platforms": {
                    "telegram": { "tool_progress": "verbose" }
                }
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "telegram", "tool_progress", None),
            json!("verbose")
        );
    }

    #[test]
    fn global_setting_when_no_platform_override() {
        let config = json!({
            "display": {
                "tool_progress": "new",
                "platforms": {}
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "telegram", "tool_progress", None),
            json!("new")
        );
    }

    #[test]
    fn platform_override_only_affects_that_platform() {
        let config = json!({
            "display": {
                "tool_progress": "all",
                "platforms": {
                    "slack": { "tool_progress": "off" }
                }
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "slack", "tool_progress", None),
            json!("off")
        );
        assert_eq!(
            resolve_display_setting(&config, "telegram", "tool_progress", None),
            json!("all")
        );
    }

    #[test]
    fn legacy_overrides_read() {
        let config = json!({
            "display": {
                "tool_progress": "all",
                "tool_progress_overrides": {
                    "signal": "off",
                    "telegram": "verbose"
                }
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "signal", "tool_progress", None),
            json!("off")
        );
        assert_eq!(
            resolve_display_setting(&config, "telegram", "tool_progress", None),
            json!("verbose")
        );
    }

    #[test]
    fn tool_progress_false_normalised_to_off() {
        let config = json!({
            "display": {
                "tool_progress": false
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "telegram", "tool_progress", None),
            json!("off")
        );
    }

    #[test]
    fn only_long_running_visibility_accepts_generic_mode() {
        let config = json!({
            "display": {
                "platforms": {
                    "whatsapp": {
                        "thinking_progress": "generic",
                        "interim_assistant_messages": "generic",
                        "long_running_notifications": "generic"
                    }
                }
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "whatsapp", "thinking_progress", None),
            json!(false)
        );
        assert_eq!(
            resolve_display_setting(&config, "whatsapp", "interim_assistant_messages", None),
            json!(false)
        );
        assert_eq!(
            resolve_display_setting(&config, "whatsapp", "long_running_notifications", None),
            json!("generic")
        );
    }

    #[test]
    fn thinking_progress_string_false_normalised_to_false() {
        let config = json!({
            "display": {
                "platforms": {
                    "whatsapp": {
                        "thinking_progress": "false"
                    }
                }
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "whatsapp", "thinking_progress", None),
            json!(false)
        );
    }

    #[test]
    fn high_tier_platforms() {
        let empty = json!({});
        assert_eq!(
            resolve_display_setting(&empty, "telegram", "tool_progress", None),
            json!("off")
        );
        assert_eq!(
            resolve_display_setting(&empty, "discord", "tool_progress", None),
            json!("all")
        );
    }

    #[test]
    fn low_tier_platforms() {
        let empty = json!({});
        for plat in [
            "signal",
            "bluebubbles",
            "weixin",
            "wecom",
            "dingtalk",
            "whatsapp_cloud",
        ] {
            assert_eq!(
                resolve_display_setting(&empty, plat, "tool_progress", None),
                json!("off"),
                "platform: {plat}"
            );
        }
    }

    #[test]
    fn telegram_mobile_chatter_defaults() {
        let empty = json!({});
        assert_eq!(
            resolve_display_setting(&empty, "telegram", "interim_assistant_messages", None),
            json!(true)
        );
        assert_eq!(
            resolve_display_setting(&empty, "telegram", "long_running_notifications", None),
            json!(true)
        );
        assert_eq!(
            resolve_display_setting(&empty, "telegram", "busy_ack_detail", None),
            json!(false)
        );
        assert_eq!(
            resolve_display_setting(&empty, "discord", "interim_assistant_messages", None),
            json!(true)
        );
        assert_eq!(
            resolve_display_setting(&empty, "discord", "long_running_notifications", None),
            json!(true)
        );
        assert_eq!(
            resolve_display_setting(&empty, "discord", "busy_ack_detail", None),
            json!(true)
        );
    }

    #[test]
    fn slack_workspace_chatter_defaults() {
        let empty = json!({});
        assert_eq!(
            resolve_display_setting(&empty, "slack", "tool_progress", None),
            json!("off")
        );
        assert_eq!(
            resolve_display_setting(&empty, "slack", "long_running_notifications", None),
            json!(false)
        );
        assert_eq!(
            resolve_display_setting(&empty, "slack", "busy_ack_detail", None),
            json!(false)
        );
    }

    #[test]
    fn streaming_per_platform() {
        let config = json!({
            "display": {
                "platforms": {
                    "telegram": { "streaming": false }
                }
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "telegram", "streaming", None),
            json!(false)
        );

        let empty = json!({});
        assert_eq!(
            resolve_display_setting(&empty, "wecom", "streaming", None),
            json!(true)
        );
        assert_eq!(
            resolve_display_setting(&empty, "wecom_callback", "streaming", None),
            json!(false)
        );
        assert_eq!(
            resolve_display_setting(&empty, "whatsapp", "streaming", None),
            Value::Null
        );
        assert_eq!(
            resolve_display_setting(&empty, "telegram", "streaming", None),
            Value::Null
        );

        let wecom_disabled = json!({
            "display": {
                "platforms": {
                    "wecom": { "streaming": false }
                }
            }
        });
        assert_eq!(
            resolve_display_setting(&wecom_disabled, "wecom", "streaming", None),
            json!(false)
        );

        // Global display.streaming is skipped for gateway resolution.
        let global_streaming = json!({
            "display": {
                "streaming": true
            }
        });
        assert_eq!(
            resolve_display_setting(&global_streaming, "telegram", "streaming", None),
            Value::Null
        );
    }

    #[test]
    fn cleanup_progress_defaults_and_normalisation() {
        let empty = json!({});
        for plat in ["telegram", "discord", "slack", "email"] {
            assert_eq!(
                resolve_display_setting(&empty, plat, "cleanup_progress", None),
                json!(false)
            );
        }

        for val in ["true", "yes", "on", "1"] {
            let config = json!({
                "display": {
                    "platforms": {
                        "telegram": { "cleanup_progress": val }
                    }
                }
            });
            assert_eq!(
                resolve_display_setting(&config, "telegram", "cleanup_progress", None),
                json!(true),
                "failed for {val}"
            );
        }
    }

    #[test]
    fn tool_progress_grouping_resolution() {
        let empty = json!({});
        assert_eq!(
            resolve_display_setting(&empty, "telegram", "tool_progress_grouping", None),
            json!("accumulate")
        );

        let config = json!({
            "display": {
                "tool_progress_grouping": "separate"
            }
        });
        assert_eq!(
            resolve_display_setting(&config, "discord", "tool_progress_grouping", None),
            json!("separate")
        );
    }

    #[test]
    fn reasoning_style_resolution() {
        let empty = json!({});
        assert_eq!(
            resolve_display_setting(&empty, "discord", "reasoning_style", None),
            json!("subtext")
        );

        for plat in ["telegram", "slack", "matrix", "api_server"] {
            assert_eq!(
                resolve_display_setting(&empty, plat, "reasoning_style", None),
                json!("code"),
                "platform: {plat}"
            );
        }
    }

    #[test]
    fn live_status_resolution() {
        let empty = json!({});
        assert_eq!(
            resolve_display_setting(&empty, "slack", "live_status", None),
            json!("full")
        );

        for val in ["true", "1", "yes", "on", "all"] {
            let config = json!({
                "display": {
                    "live_status": val
                }
            });
            assert_eq!(
                resolve_display_setting(&config, "slack", "live_status", None),
                json!("full")
            );
        }

        for val in ["false", "0", "no", "off"] {
            let config = json!({
                "display": {
                    "live_status": val
                }
            });
            assert_eq!(
                resolve_display_setting(&config, "slack", "live_status", None),
                json!("off")
            );
        }

        let verb_config = json!({
            "display": {
                "live_status": "verb"
            }
        });
        assert_eq!(
            resolve_display_setting(&verb_config, "slack", "live_status", None),
            json!("verb")
        );
    }

    #[test]
    fn tool_preview_length_resolution() {
        let empty = json!({});
        assert_eq!(
            resolve_display_setting(&empty, "telegram", "tool_preview_length", None),
            json!(40)
        );
        assert_eq!(
            resolve_display_setting(&empty, "api_server", "tool_preview_length", None),
            json!(0)
        );
        assert_eq!(
            resolve_display_setting(&empty, "email", "tool_preview_length", None),
            json!(0)
        );

        let string_len = json!({
            "display": {
                "tool_preview_length": "80"
            }
        });
        assert_eq!(
            resolve_display_setting(&string_len, "telegram", "tool_preview_length", None),
            json!(80)
        );

        let invalid_len = json!({
            "display": {
                "tool_preview_length": "invalid"
            }
        });
        assert_eq!(
            resolve_display_setting(&invalid_len, "telegram", "tool_preview_length", None),
            json!(0)
        );
    }

    #[test]
    fn resolved_display_config_struct() {
        let empty = json!({});
        let telegram = ResolvedDisplayConfig::resolve(&empty, "telegram");
        assert_eq!(telegram.tool_progress, ToolProgress::Off);
        assert_eq!(
            telegram.tool_progress_grouping,
            ToolProgressGrouping::Accumulate
        );
        assert!(!telegram.show_reasoning);
        assert_eq!(telegram.reasoning_style, ReasoningStyle::Code);
        assert_eq!(telegram.tool_preview_length, 40);
        assert_eq!(telegram.streaming, None);
        assert!(telegram.interim_assistant_messages);
        assert_eq!(
            telegram.long_running_notifications,
            LongRunningNotifications::On
        );
        assert!(!telegram.busy_ack_detail);
        assert!(telegram.busy_steer_ack_enabled);
        assert!(!telegram.cleanup_progress);
        assert_eq!(telegram.live_status, LiveStatus::Full);

        let discord = ResolvedDisplayConfig::resolve(&empty, "discord");
        assert_eq!(discord.tool_progress, ToolProgress::All);
        assert_eq!(discord.reasoning_style, ReasoningStyle::Subtext);
        assert!(discord.busy_ack_detail);

        let wecom = ResolvedDisplayConfig::resolve(&empty, "wecom");
        assert_eq!(wecom.tool_progress, ToolProgress::Off);
        assert_eq!(wecom.streaming, Some(true));
        assert!(!wecom.interim_assistant_messages);
    }

    #[test]
    fn overrideable_keys_list() {
        assert!(is_overrideable_key("tool_progress"));
        assert!(is_overrideable_key("streaming"));
        assert!(is_overrideable_key("live_status"));
        assert!(!is_overrideable_key("unknown_key"));
    }
}
