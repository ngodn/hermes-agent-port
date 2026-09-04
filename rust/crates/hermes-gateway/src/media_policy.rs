//! Port of gateway/media_policy.py.
//!
// Public API is ahead of its callers (startup + standalone delivery wire it).
#![allow(dead_code)]
//!
//! Shared config->env bridge for media-delivery policy. `media::validate_
//! media_delivery_path` reads its policy from environment variables
//! (`HERMES_MEDIA_DELIVERY_STRICT`, `HERMES_MEDIA_ALLOW_DIRS`,
//! `HERMES_MEDIA_TRUST_RECENT_FILES`); this bridges the `gateway.*` config keys
//! into those vars so every delivery entrypoint (gateway startup and any
//! standalone delivery path) filters MEDIA paths under one policy.
//!
//! Precedence: an already-set environment variable WINS over config.yaml (an
//! operator shell export pins behavior), so this never overwrites a pre-existing
//! value. Best-effort: a bridge failure must never break delivery.

use serde_json::Value;

const STRICT_ENV: &str = "HERMES_MEDIA_DELIVERY_STRICT";
const ALLOW_DIRS_ENV: &str = "HERMES_MEDIA_ALLOW_DIRS";
const TRUST_RECENT_ENV: &str = "HERMES_MEDIA_TRUST_RECENT_FILES";

#[cfg(windows)]
const PATH_SEP: &str = ";";
#[cfg(not(windows))]
const PATH_SEP: &str = ":";

fn env_unset(name: &str) -> bool {
    std::env::var(name).map(|v| v.is_empty()).unwrap_or(true)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Bridge `gateway.*` media-policy settings from a loaded config into the env.
/// Idempotent and env-wins. Never panics.
pub fn apply_media_policy_env(config: &Value) {
    let Some(gateway) = config.get("gateway").filter(|g| g.is_object()) else {
        return;
    };

    // gateway.strict -> HERMES_MEDIA_DELIVERY_STRICT ("1"/"0"), only when present.
    if let Some(strict) = gateway.get("strict") {
        if !strict.is_null() && env_unset(STRICT_ENV) {
            std::env::set_var(STRICT_ENV, if truthy(strict) { "1" } else { "0" });
        }
    }

    // gateway.media_delivery_allow_dirs -> HERMES_MEDIA_ALLOW_DIRS.
    if let Some(allow_dirs) = gateway.get("media_delivery_allow_dirs") {
        if truthy(allow_dirs) && env_unset(ALLOW_DIRS_ENV) {
            let joined = match allow_dirs {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|p| match p {
                        Value::String(s) if !s.is_empty() => Some(s.clone()),
                        Value::String(_) | Value::Null => None,
                        other => Some(other.to_string()),
                    })
                    .collect::<Vec<_>>()
                    .join(PATH_SEP),
                _ => String::new(),
            };
            if !joined.is_empty() {
                std::env::set_var(ALLOW_DIRS_ENV, joined);
            }
        }
    }

    // gateway.trust_recent_files -> HERMES_MEDIA_TRUST_RECENT_FILES ("1"/"0").
    if let Some(trust) = gateway.get("trust_recent_files") {
        if !trust.is_null() && env_unset(TRUST_RECENT_ENV) {
            std::env::set_var(TRUST_RECENT_ENV, if truthy(trust) { "1" } else { "0" });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear() {
        std::env::remove_var(STRICT_ENV);
        std::env::remove_var(ALLOW_DIRS_ENV);
        std::env::remove_var(TRUST_RECENT_ENV);
    }

    #[test]
    fn bridges_config_into_env() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        let cfg = serde_json::json!({
            "gateway": {
                "strict": true,
                "media_delivery_allow_dirs": ["/srv/out", "/data/media"],
                "trust_recent_files": false
            }
        });
        apply_media_policy_env(&cfg);
        assert_eq!(std::env::var(STRICT_ENV).unwrap(), "1");
        assert_eq!(std::env::var(TRUST_RECENT_ENV).unwrap(), "0");
        let allow = std::env::var(ALLOW_DIRS_ENV).unwrap();
        assert!(allow.contains("/srv/out") && allow.contains("/data/media"));
        clear();
    }

    #[test]
    fn env_wins_over_config() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        std::env::set_var(STRICT_ENV, "0"); // operator export
        let cfg = serde_json::json!({"gateway": {"strict": true}});
        apply_media_policy_env(&cfg);
        assert_eq!(
            std::env::var(STRICT_ENV).unwrap(),
            "0",
            "existing env preserved"
        );
        clear();
    }

    #[test]
    fn absent_gateway_is_noop() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        apply_media_policy_env(&serde_json::json!({}));
        assert!(env_unset(STRICT_ENV));
        clear();
    }

    #[test]
    fn string_allow_dirs_passthrough() {
        let _g = ENV_LOCK.lock().unwrap();
        clear();
        let cfg = serde_json::json!({"gateway": {"media_delivery_allow_dirs": "/one/dir"}});
        apply_media_policy_env(&cfg);
        assert_eq!(std::env::var(ALLOW_DIRS_ENV).unwrap(), "/one/dir");
        clear();
    }
}
