//! Gemini thinking-config translation and output-cap raising.
//!
//! Port of the Gemini reasoning helpers split across two Python modules:
//!
//! - `agent/transports/chat_completions.py`: `_build_gemini_thinking_config`,
//!   `_snake_case_gemini_thinking_config`, `_raise_gemini_thinking_max_tokens`.
//! - `agent/gemini_native_adapter.py`: `_normalize_thinking_config`,
//!   `_thinking_requests_output_headroom`, `_effective_gemini_max_output_tokens`.
//!
//! The one public entry point today is [`raise_output_cap`], the port of the
//! `chat_completions._raise_gemini_thinking_max_tokens` wrapper: it builds the
//! Gemini thinking config for a model + reasoning config and, only when that
//! config is nonempty, runs the requested `max_tokens` through the effective-cap
//! resolver. A nonempty-but-disabled config (`{"includeThoughts": false}`) still
//! reaches the resolver, which is why a disabled reasoning config with a `None`
//! request still yields the Gemini default ceiling rather than passing `None`
//! through.
//!
//! The other helpers ([`build_gemini_thinking_config`],
//! [`snake_case_gemini_thinking_config`], [`normalize_thinking_config`],
//! [`thinking_requests_output_headroom`], [`effective_gemini_max_output_tokens`])
//! are exposed for the future native Gemini REST transport (`build_gemini_request`
//! uses the normalize + effective pair directly). They are unused by the gateway
//! today, so the module carries `allow(dead_code)`.
//!
//! Integer coercion shares the port's i64/u64 JSON boundary. Values within that
//! range are preserved exactly; Python integers beyond it remain unsupported.
#![allow(dead_code)]

use crate::python_value::{integer, python_repr, python_whitespace, truthy};
use serde_json::{json, Map, Value};

/// Default output-token ceiling in the Python reference.
/// Mirrors `GEMINI_DEFAULT_MAX_OUTPUT_TOKENS` in `gemini_native_adapter.py`.
pub const GEMINI_DEFAULT_MAX_OUTPUT_TOKENS: i64 = 65535;

/// Python `str.strip()` with no argument: Unicode whitespace plus the four
/// information separators (see `python_value::python_whitespace`).
fn strip(text: &str) -> &str {
    text.trim_matches(python_whitespace)
}

/// Python `str(value)`: strings pass through unquoted, everything else renders
/// as `repr` does. `str` and `repr` only diverge for the top-level `str` type,
/// so reusing `python_repr` for the non-string arms is faithful.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => python_repr(other),
    }
}

/// Python `int(value)` acceptance for the scalar types `_effective` cares about:
/// `bool`/`int`/`float`/`str` coerce, everything else (`None`, list, dict) is a
/// `TypeError`/`ValueError` and returns `None`. Delegates to
/// `python_value::integer`, which truncates floats toward zero and parses
/// strings with Python's digit/underscore rules.
fn py_int(value: &Value) -> Option<Value> {
    integer(value)
}

/// Port of `_build_gemini_thinking_config`. Translates a Hermes/OpenRouter-style
/// reasoning config to a Gemini `thinkingConfig` object, or `None` when the
/// field must be omitted (non-dict config, or a non-Gemini model that would
/// reject the parameter with HTTP 400).
pub fn build_gemini_thinking_config(
    model: &str,
    reasoning_config: Option<&Value>,
) -> Option<Value> {
    // ``reasoning_config is None or not isinstance(reasoning_config, dict)``.
    let config = match reasoning_config {
        Some(Value::Object(map)) => map,
        _ => return None,
    };

    // ``(model or "").strip().lower()`` then strip a ``google/`` aggregator prefix.
    let mut normalized_model = strip(model).to_lowercase();
    if let Some(rest) = normalized_model.strip_prefix("google/") {
        normalized_model = rest.to_string();
    }

    // thinking_config is a Gemini-only field; Gemma/PaLM reject it (#17426).
    if !normalized_model.starts_with("gemini") {
        return None;
    }

    // ``reasoning_config.get("enabled") is False``: an exact bool False only.
    if config.get("enabled") == Some(&Value::Bool(false)) {
        return Some(json!({"includeThoughts": false}));
    }

    // ``str(reasoning_config.get("effort", "medium") or "medium").strip().lower()``.
    let effort_source = match config.get("effort") {
        None => "medium".to_string(),
        Some(value) if truthy(value) => py_str(value),
        Some(_) => "medium".to_string(),
    };
    let mut effort = strip(&effort_source).to_lowercase();
    if effort == "none" {
        return Some(json!({"includeThoughts": false}));
    }

    let mut thinking_config = Map::new();
    thinking_config.insert("includeThoughts".to_string(), Value::Bool(true));

    // Gemini 2.5 accepts thinkingBudget but we don't guess one; includeThoughts
    // alone surfaces thought parts without risking validation errors.
    if normalized_model.starts_with("gemini-2.5-") {
        return Some(Value::Object(thinking_config));
    }

    const KNOWN_EFFORTS: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
    if !KNOWN_EFFORTS.contains(&effort.as_str()) {
        effort = "medium".to_string();
    }

    // Gemini 3 (incl. 3.1) clamps Hermes' wider effort set to documented levels.
    // ``startswith(("gemini-3", "gemini-3.1"))`` is just ``startswith("gemini-3")``.
    if normalized_model.starts_with("gemini-3") {
        let high_set = matches!(effort.as_str(), "high" | "xhigh" | "max" | "ultra");
        if normalized_model.contains("flash") {
            let level = if matches!(effort.as_str(), "minimal" | "low") {
                "low"
            } else if high_set {
                "high"
            } else {
                "medium"
            };
            thinking_config.insert("thinkingLevel".to_string(), Value::String(level.into()));
        } else if normalized_model.contains("pro") {
            let level = if high_set { "high" } else { "low" };
            thinking_config.insert("thinkingLevel".to_string(), Value::String(level.into()));
        }
    }

    Some(Value::Object(thinking_config))
}

/// Port of `_snake_case_gemini_thinking_config`. Converts Gemini camelCase
/// thinking-config keys to the OpenAI-compat snake_case field names.
pub fn snake_case_gemini_thinking_config(config: Option<&Value>) -> Option<Value> {
    let config = match config {
        Some(Value::Object(map)) if !map.is_empty() => map,
        _ => return None,
    };

    let mut translated = Map::new();
    if let Some(Value::Bool(include)) = config.get("includeThoughts") {
        translated.insert("include_thoughts".to_string(), Value::Bool(*include));
    }
    if let Some(Value::String(level)) = config.get("thinkingLevel") {
        let stripped = strip(level);
        if !stripped.is_empty() {
            translated.insert(
                "thinking_level".to_string(),
                Value::String(stripped.to_lowercase()),
            );
        }
    }
    // ``isinstance(x, (int, float))`` includes bool in Python, so int(True) == 1.
    if let Some(value @ (Value::Bool(_) | Value::Number(_))) = config.get("thinkingBudget") {
        if let Some(budget) = py_int(value) {
            translated.insert("thinking_budget".to_string(), budget);
        }
    }

    if translated.is_empty() {
        None
    } else {
        Some(Value::Object(translated))
    }
}

/// Port of `_normalize_thinking_config`. Accepts either camelCase or snake_case
/// keys (camelCase wins when both are present) and returns a normalized
/// camelCase config, or `None` when nothing usable is present.
pub fn normalize_thinking_config(config: Option<&Value>) -> Option<Value> {
    let config = match config {
        Some(Value::Object(map)) if !map.is_empty() => map,
        _ => return None,
    };

    // ``config.get("thinkingBudget", config.get("thinking_budget"))``: the
    // camelCase key wins whenever it is present (even with a null value); the
    // snake_case value is only the fallback default when the key is absent.
    let budget = config
        .get("thinkingBudget")
        .or_else(|| config.get("thinking_budget"));
    let include = config
        .get("includeThoughts")
        .or_else(|| config.get("include_thoughts"));
    let level = config
        .get("thinkingLevel")
        .or_else(|| config.get("thinking_level"));

    let mut normalized = Map::new();
    // ``isinstance(budget, (int, float))`` includes bool.
    if let Some(value @ (Value::Bool(_) | Value::Number(_))) = budget {
        if let Some(coerced) = py_int(value) {
            normalized.insert("thinkingBudget".to_string(), coerced);
        }
    }
    if let Some(Value::Bool(value)) = include {
        normalized.insert("includeThoughts".to_string(), Value::Bool(*value));
    }
    if let Some(Value::String(value)) = level {
        let stripped = strip(value);
        if !stripped.is_empty() {
            normalized.insert(
                "thinkingLevel".to_string(),
                Value::String(stripped.to_lowercase()),
            );
        }
    }

    if normalized.is_empty() {
        None
    } else {
        Some(Value::Object(normalized))
    }
}

/// Port of `_thinking_requests_output_headroom`. Returns true when Gemini will
/// spend output tokens on thinking, so the caller must raise a too-small cap.
pub fn thinking_requests_output_headroom(thinking_config: Option<&Value>) -> bool {
    let normalized = match normalize_thinking_config(thinking_config) {
        Some(Value::Object(map)) => map,
        _ => return false,
    };

    // ``normalized.get("includeThoughts") is False``.
    if normalized.get("includeThoughts") == Some(&Value::Bool(false)) {
        // ``"thinkingLevel" in normalized or bool(normalized.get("thinkingBudget"))``.
        let has_level = normalized.contains_key("thinkingLevel");
        let budget_truthy = normalized.get("thinkingBudget").is_some_and(truthy);
        return has_level || budget_truthy;
    }

    // normalize only stores thinkingBudget when it coerced to an int, so this is
    // always the ``isinstance(budget, int)`` branch when the key is present.
    if let Some(budget) = normalized.get("thinkingBudget") {
        let non_positive = budget.as_i64().is_some_and(|n| n <= 0);
        if non_positive && !normalized.contains_key("thinkingLevel") {
            return false;
        }
    }
    true
}

/// Port of `_effective_gemini_max_output_tokens`. Resolves the native
/// `maxOutputTokens`, raising a too-small explicit cap to the Gemini ceiling
/// when thinking is enabled. `max_tokens` is `&Value` so Python's `Optional[int]`
/// maps onto `Value::Null` (the `None` case), while the wrapper's arbitrary
/// `requested` value flows straight through the `int(...)` coercion.
pub fn effective_gemini_max_output_tokens(
    max_tokens: &Value,
    thinking_config: Option<&Value>,
) -> Value {
    let Some(requested) = py_int(max_tokens) else {
        return json!(GEMINI_DEFAULT_MAX_OUTPUT_TOKENS);
    };
    if requested.as_i64().is_some_and(|n| n <= 0) {
        return json!(GEMINI_DEFAULT_MAX_OUTPUT_TOKENS);
    }
    if thinking_requests_output_headroom(thinking_config)
        && requested
            .as_u64()
            .is_some_and(|n| n < GEMINI_DEFAULT_MAX_OUTPUT_TOKENS as u64)
    {
        return json!(GEMINI_DEFAULT_MAX_OUTPUT_TOKENS);
    }
    requested
}

/// Port of `_raise_gemini_thinking_max_tokens`. Public entry point: raise Gemini
/// output caps that thinking tokens would otherwise consume. When the model +
/// reasoning config yields no thinking config (`None`), the original `requested`
/// value is returned unchanged; a nonempty config (including the disabled
/// `{"includeThoughts": false}` form) runs `requested` through the effective-cap
/// resolver, so the coerced integer / default ceiling is returned as a number.
pub fn raise_output_cap(model: &str, reasoning_config: Option<&Value>, requested: &Value) -> Value {
    let thinking_config = build_gemini_thinking_config(model, reasoning_config);
    // ``if not thinking_config``: build never returns an empty dict, so this is
    // purely the ``None`` case. A nonempty disabled config proceeds.
    let Some(thinking_config) = thinking_config else {
        return requested.clone();
    };
    effective_gemini_max_output_tokens(requested, Some(&thinking_config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct WrapperCase {
        name: String,
        model: String,
        reasoning_config: Value,
        requested: Value,
        build: Value,
        snake: Value,
        raised: Value,
    }

    #[derive(serde::Deserialize)]
    struct ConfigCase {
        name: String,
        thinking_config: Value,
        normalized: Value,
        headroom: bool,
    }

    #[derive(serde::Deserialize)]
    struct EffectiveCase {
        name: String,
        max_tokens: Value,
        thinking_config: Value,
        expected: Value,
    }

    #[derive(serde::Deserialize)]
    struct Goldens {
        wrapper: Vec<WrapperCase>,
        configs: Vec<ConfigCase>,
        effective: Vec<EffectiveCase>,
    }

    /// Python `None` (JSON null) means "no reasoning config"; a real dict is
    /// passed by reference. Mirrors the caller handing `Optional[dict]`.
    fn as_config(value: &Value) -> Option<&Value> {
        if value.is_null() {
            None
        } else {
            Some(value)
        }
    }

    fn goldens() -> Goldens {
        let raw = include_str!("../../../tools/gemini-thinking-goldens.json");
        serde_json::from_str(raw).expect("golden JSON parses")
    }

    #[test]
    fn matches_wrapper_goldens() {
        let goldens = goldens();
        assert!(!goldens.wrapper.is_empty(), "expected wrapper cases");
        for case in &goldens.wrapper {
            let built =
                build_gemini_thinking_config(&case.model, as_config(&case.reasoning_config));
            let built_value = built.clone().unwrap_or(Value::Null);
            assert_eq!(built_value, case.build, "build `{}`", case.name);

            let snake = snake_case_gemini_thinking_config(built.as_ref()).unwrap_or(Value::Null);
            assert_eq!(snake, case.snake, "snake `{}`", case.name);

            let raised = raise_output_cap(
                &case.model,
                as_config(&case.reasoning_config),
                &case.requested,
            );
            assert_eq!(raised, case.raised, "raised `{}`", case.name);
        }
    }

    #[test]
    fn matches_config_goldens() {
        let goldens = goldens();
        assert!(!goldens.configs.is_empty(), "expected config cases");
        for case in &goldens.configs {
            let normalized =
                normalize_thinking_config(Some(&case.thinking_config)).unwrap_or(Value::Null);
            assert_eq!(normalized, case.normalized, "normalize `{}`", case.name);

            let headroom = thinking_requests_output_headroom(Some(&case.thinking_config));
            assert_eq!(headroom, case.headroom, "headroom `{}`", case.name);
        }
    }

    #[test]
    fn matches_effective_goldens() {
        let goldens = goldens();
        assert!(!goldens.effective.is_empty(), "expected effective cases");
        for case in &goldens.effective {
            let got = effective_gemini_max_output_tokens(
                &case.max_tokens,
                as_config(&case.thinking_config),
            );
            assert_eq!(got, case.expected, "effective `{}`", case.name);
        }
    }

    #[test]
    fn disabled_config_still_reaches_effective_cap() {
        // {"includeThoughts": false} is nonempty, so a None request still resolves
        // to the ceiling instead of passing None through unchanged.
        let config = json!({"enabled": false});
        let raised = raise_output_cap("gemini-2.5-flash", Some(&config), &Value::Null);
        assert_eq!(raised, json!(GEMINI_DEFAULT_MAX_OUTPUT_TOKENS));
        // A valid explicit cap with no headroom passes through coerced.
        let raised = raise_output_cap("gemini-2.5-flash", Some(&config), &json!(1024));
        assert_eq!(raised, json!(1024));
    }

    #[test]
    fn none_thinking_config_passes_requested_through_unchanged() {
        // Non-Gemini model yields no config, so the arbitrary requested value is
        // returned verbatim (type preserved), not coerced.
        assert_eq!(
            raise_output_cap("gpt-4", Some(&json!({"effort": "high"})), &json!("keep-me")),
            json!("keep-me")
        );
        assert_eq!(
            raise_output_cap("gemini-3-flash", None, &Value::Null),
            Value::Null
        );
    }

    #[test]
    fn effective_preserves_unsigned_json_integers() {
        // Raising a minimum must never lower a larger representable value.
        let huge = json!(u64::MAX);
        let got =
            effective_gemini_max_output_tokens(&huge, Some(&json!({"thinkingLevel": "high"})));
        assert_eq!(got, huge);
    }
}
