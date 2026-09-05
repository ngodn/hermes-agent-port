//! Pure custom-provider `extra_body` selection.
//!
//! Port of `_normalized_custom_base_url`, `_custom_provider_model_matches`, and
//! `_custom_provider_extra_body_for_agent` from `agent/agent_init.py`. Given an
//! agent's provider / model / base_url and the list of custom-provider config
//! entries, [`select_extra_body`] returns the per-provider `extra_body` request
//! overrides that apply (a copy of the winning entry's map), or `None`.
//!
//! Scope: the caller supplies `entries` already normalized to the legacy list
//! shape, one map per provider with the optional keys `provider_key`, `name`,
//! `base_url`, `model`, `models`, and `extra_body`. Turning the v12+ unified
//! `providers` config into that legacy shape is a separate concern that stays
//! with the caller. This module is pure: it only reads the values it is handed
//! and never issues a request or infers a model.
#![allow(dead_code)]

use crate::python_value::{python_repr, python_whitespace, truthy};
use serde_json::{Map, Value};

const NULL: Value = Value::Null;

/// Python `str(value)`: strings pass through unquoted, everything else renders
/// exactly as `repr` does (containers, numbers, bools, `None`). `str` and
/// `repr` only diverge for the top-level `str` type, so reusing `python_repr`
/// for the non-string arms is faithful.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => python_repr(other),
    }
}

/// Python `str(value or "")`: falsy values collapse to `""` before `str`.
fn py_str_or_empty(value: &Value) -> String {
    if truthy(value) {
        py_str(value)
    } else {
        String::new()
    }
}

/// Python `str.strip()` with no argument (whitespace plus the four information
/// separators, per `python_whitespace`).
fn strip(text: &str) -> &str {
    text.trim_matches(python_whitespace)
}

/// Python `_normalized_custom_base_url` for a `str` input: `strip()` first
/// (both ends), then `rstrip("/")` (trailing slashes only).
fn normalized_base_url_str(value: &str) -> String {
    strip(value).trim_end_matches('/').to_string()
}

/// Python `_normalized_custom_base_url` for an arbitrary config value: a
/// non-`str` returns `""` (matching the `isinstance(value, str)` guard).
fn normalized_base_url_value(value: &Value) -> String {
    match value {
        Value::String(text) => normalized_base_url_str(text),
        _ => String::new(),
    }
}

/// Port of `_custom_provider_model_matches`.
fn model_matches(agent_model: &str, entry: &Map<String, Value>) -> bool {
    let agent_model_norm = strip(agent_model).to_lowercase();

    // Multi-model entries (`models:` mapping or list): matching ANY catalog
    // entry counts, so a provider whose `model` differs from the session model
    // still contributes its per-provider request settings.
    let mut catalog: Vec<String> = Vec::new();
    match entry.get("models") {
        // Object keys are always strings, so `str(k)` is the key itself.
        Some(Value::Object(models)) => {
            for key in models.keys() {
                catalog.push(strip(key).to_lowercase());
            }
        }
        // List/tuple elements are coerced with a plain `str(m)` (no `or ""`).
        Some(Value::Array(models)) => {
            for model in models {
                catalog.push(strip(&py_str(model)).to_lowercase());
            }
        }
        _ => {}
    }
    if !catalog.is_empty() && catalog.contains(&agent_model_norm) {
        return true;
    }

    let provider_model =
        strip(&py_str_or_empty(entry.get("model").unwrap_or(&NULL))).to_lowercase();
    if provider_model.is_empty() && catalog.is_empty() {
        return true;
    }
    provider_model == agent_model_norm
}

/// Port of `_custom_provider_extra_body_for_agent`.
///
/// `entries` is the caller-normalized custom-provider list. A value that is not
/// a JSON array is treated as empty. Python can raise for non-iterable invalid
/// inputs; callers here supply a normalized list.
pub fn select_extra_body(
    provider: &str,
    model: &str,
    base_url: &str,
    entries: &Value,
) -> Option<Map<String, Value>> {
    let provider_norm = strip(provider).to_lowercase();
    let provider_key_filter: String = if provider_norm == "custom" {
        String::new()
    } else if let Some(rest) = provider_norm.strip_prefix("custom:") {
        // `split(":", 1)[1].strip()`: everything after the first colon, then
        // whitespace-stripped. Already lowercased with the whole string above.
        strip(rest).to_string()
    } else {
        return None;
    };

    let target_url = normalized_base_url_str(base_url);
    if target_url.is_empty() {
        return None;
    }

    let entries = match entries {
        Value::Array(entries) => entries.as_slice(),
        _ => &[],
    };

    let mut fallback: Option<Map<String, Value>> = None;
    for entry in entries {
        let Value::Object(entry) = entry else {
            continue;
        };
        if !provider_key_filter.is_empty() {
            let provider_key =
                strip(&py_str_or_empty(entry.get("provider_key").unwrap_or(&NULL))).to_lowercase();
            let name = strip(&py_str_or_empty(entry.get("name").unwrap_or(&NULL))).to_lowercase();
            if provider_key_filter != provider_key && provider_key_filter != name {
                continue;
            }
        }
        if normalized_base_url_value(entry.get("base_url").unwrap_or(&NULL)) != target_url {
            continue;
        }
        let extra_body = match entry.get("extra_body") {
            Some(Value::Object(map)) if !map.is_empty() => map,
            _ => continue,
        };
        // Plain `.strip()` here, no `.lower()`: this only gates match-vs-fallback.
        let provider_model =
            strip(&py_str_or_empty(entry.get("model").unwrap_or(&NULL))).to_string();
        if !provider_model.is_empty() {
            if model_matches(model, entry) {
                return Some(extra_body.clone());
            }
        } else if fallback.is_none() {
            fallback = Some(extra_body.clone());
        }
    }

    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(serde::Deserialize)]
    struct Golden {
        name: String,
        provider: String,
        model: String,
        base_url: String,
        entries: Value,
        expected: Option<Map<String, Value>>,
    }

    #[test]
    fn matches_python_goldens() {
        let raw = include_str!("../../../tools/custom-request-goldens.json");
        let goldens: Vec<Golden> = serde_json::from_str(raw).expect("golden JSON parses");
        assert!(!goldens.is_empty(), "expected at least one golden case");
        for case in &goldens {
            let got = select_extra_body(&case.provider, &case.model, &case.base_url, &case.entries);
            assert_eq!(got, case.expected, "case `{}`", case.name);
        }
    }

    #[test]
    fn base_url_normalization_edge_cases() {
        assert_eq!(normalized_base_url_str("  http://x///  "), "http://x");
        assert_eq!(normalized_base_url_str("http://x/"), "http://x");
        // strip() runs before rstrip("/"), so an interior trailing space is kept
        // once the outer whitespace is removed and only the last slash is cut.
        assert_eq!(normalized_base_url_str("http://x/ /"), "http://x/ ");
        assert_eq!(normalized_base_url_value(&json!(123)), "");
        assert_eq!(normalized_base_url_value(&Value::Null), "");
    }

    #[test]
    fn non_array_entries_are_empty() {
        let entries = json!({"base_url": "http://x", "extra_body": {"a": 1}});
        assert_eq!(select_extra_body("custom", "m", "http://x", &entries), None);
        assert_eq!(
            select_extra_body("custom", "m", "http://x", &Value::Null),
            None
        );
    }

    #[test]
    fn model_match_returns_immediately_over_later_fallback() {
        // A matching-model entry short-circuits the loop, so a later plain
        // fallback entry is never consulted.
        let entries = json!([
            {"model": "gpt-4", "base_url": "http://x", "extra_body": {"tier": "flex"}},
            {"base_url": "http://x", "extra_body": {"tier": "default"}},
        ]);
        let got = select_extra_body("custom", "gpt-4", "http://x", &entries);
        assert_eq!(
            got,
            Some(json!({"tier": "flex"}).as_object().unwrap().clone())
        );
    }
}
