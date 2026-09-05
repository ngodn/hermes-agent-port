//! Shared reasoning vocabulary and config resolution from hermes_constants.py
//! and agent/reasoning_effort.py. Provider wire shapes stay in their profiles.
use crate::python_value::python_whitespace;
use serde_json::{json, Value};

pub const EFFORT_LADDER: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Preserve supported and bespoke spellings. Otherwise choose the nearest
/// weaker enabled level, falling back to the provider's minimum enabled level.
pub fn clamp_effort(
    effort: Option<&str>,
    supported: &[String],
    overrides: Option<&serde_json::Map<String, Value>>,
) -> Option<String> {
    let original = effort.map(str::to_owned);
    let requested = effort
        .unwrap_or("")
        .trim_matches(python_whitespace)
        .to_lowercase();
    if requested.is_empty() || supported.is_empty() {
        return original;
    }
    let normalized: Vec<String> = supported
        .iter()
        .map(|s| s.trim_matches(python_whitespace).to_lowercase())
        .filter(|s| EFFORT_LADDER.contains(&s.as_str()))
        .collect();
    if normalized.is_empty() || normalized.contains(&requested) {
        return original;
    }
    if let Some(mapped) = overrides
        .and_then(|map| map.get(&requested))
        .and_then(Value::as_str)
    {
        if normalized.iter().any(|s| s == mapped) {
            return Some(mapped.into());
        }
    }
    let Some(index) = EFFORT_LADDER.iter().position(|s| *s == requested) else {
        return original;
    };
    let candidates: Vec<usize> = normalized
        .iter()
        .filter(|s| s.as_str() != "none")
        .map(|s| EFFORT_LADDER.iter().position(|v| v == s).unwrap())
        .collect();
    candidates
        .iter()
        .copied()
        .filter(|i| *i < index)
        .max()
        .or_else(|| candidates.iter().copied().min())
        .map(|i| EFFORT_LADDER[i].to_owned())
        .or(original)
}

/// Chat transport normalization precedes provider-specific clamping. Preserve
/// the original config unless the shared wire vocabulary changes the effort.
pub fn for_chat_wire(config: Option<&Value>) -> Option<Value> {
    let config = config?;
    let Some(map) = config.as_object() else {
        return Some(config.clone());
    };
    let effort = map
        .get("effort")
        .filter(|v| crate::python_value::truthy(v))
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(v))
        })
        .unwrap_or_default()
        .trim_matches(python_whitespace)
        .to_lowercase();
    if effort.is_empty() {
        return Some(config.clone());
    }
    let supported: Vec<String> = EFFORT_LADDER
        .iter()
        .copied()
        .filter(|v| *v != "ultra")
        .map(str::to_owned)
        .collect();
    let clamped = clamp_effort(Some(&effort), &supported, None).unwrap();
    if clamped == effort {
        return Some(config.clone());
    }
    let mut normalized = map.clone();
    normalized.insert("effort".into(), Value::String(clamped));
    Some(Value::Object(normalized))
}

fn parse(effort: &Value) -> Option<Value> {
    if effort == &Value::Bool(false) {
        return Some(json!({"enabled": false}));
    }
    let effort = effort
        .as_str()?
        .trim_matches(python_whitespace)
        .to_lowercase();
    if ["none", "false", "disabled"].contains(&effort.as_str()) {
        return Some(json!({"enabled": false}));
    }
    EFFORT_LADDER
        .contains(&effort.as_str())
        .then(|| json!({"enabled": true, "effort": effort}))
}

fn add(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
}

/// Python re.sub consumes both digits in a match, so adjacent version pairs
/// overlap and must not both be rewritten in the same substitution pass.
fn version_separator(value: &str, from: char, to: char) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut result = String::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len()
            && chars[i + 1] == from
            && crate::python_value::decimal_digit(chars[i]).is_some()
            && crate::python_value::decimal_digit(chars[i + 2]).is_some()
        {
            result.extend([chars[i], to, chars[i + 2]]);
            i += 3;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

fn derivatives(values: &mut Vec<String>, model: &str) {
    let dashed = model.replace('.', "-");
    let dotted = model.replace('-', ".");
    for value in [
        model.to_owned(),
        dashed.clone(),
        dotted.clone(),
        version_separator(model, '-', '.'),
        version_separator(model, '.', '-'),
        version_separator(&dashed, '-', '.'),
        version_separator(&dotted, '.', '-'),
    ] {
        add(values, value);
    }
}

fn variants(model: &str) -> Vec<String> {
    let mut values = Vec::new();
    derivatives(&mut values, model);
    let parts: Vec<&str> = model.split('/').collect();
    if parts.len() >= 2 {
        derivatives(&mut values, parts.last().unwrap());
    }
    if parts.len() >= 3 {
        derivatives(&mut values, &parts[1..].join("/"));
    }
    for bare in values.clone().into_iter().filter(|v| !v.contains('/')) {
        for provider in [
            "anthropic",
            "openai",
            "google",
            "openrouter",
            "groq",
            "mistral",
            "xai",
            "cohere",
            "perplexity",
            "together",
            "fireworks",
            "deepseek",
        ] {
            add(&mut values, format!("{provider}/{bare}"));
        }
    }
    for single in values
        .clone()
        .into_iter()
        .filter(|v| v.matches('/').count() == 1)
    {
        for aggregator in ["openrouter", "opencode", "fireworks", "groq", "together"] {
            add(&mut values, format!("{aggregator}/{single}"));
        }
    }
    values
}

/// Per-model spelling variants precede the global setting. Invalid overrides
/// fall through; explicit false disables reasoning instead of restoring defaults.
pub fn resolve_config(config: &Value, model: &str) -> Option<Value> {
    let model = if model.is_empty() {
        let configured = &config["model"];
        match configured {
            Value::String(value) => value.trim_matches(python_whitespace).to_owned(),
            Value::Object(_) => {
                let value = [&configured["default"], &configured["model"]]
                    .into_iter()
                    .find(|v| crate::python_value::truthy(v));
                value
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| crate::python_value::python_repr(v))
                    })
                    .unwrap_or_default()
                    .trim_matches(python_whitespace)
                    .to_owned()
            }
            _ => String::new(),
        }
    } else {
        model.to_owned()
    };
    let agent = &config["agent"];
    if let Some(overrides) = agent["reasoning_overrides"].as_object() {
        for variant in variants(&model) {
            if let Some(result) = overrides.get(&variant).and_then(parse) {
                return Some(result);
            }
        }
    }
    parse(&agent["reasoning_effort"])
}

#[cfg(test)]
mod tests {
    #[test]
    fn chat_wire_normalization_matches_python_without_mutating_config() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/wire-reasoning-goldens.json"))
                .unwrap();
        for row in cases.as_array().unwrap() {
            assert_eq!(
                super::for_chat_wire(Some(&row["config"])).unwrap(),
                row["result"],
                "{row}"
            );
        }
        assert_eq!(super::for_chat_wire(None), None);
    }

    use super::*;
    #[test]
    fn python_clamping_and_config_oracles() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/upstage-goldens.json")).unwrap();
        for row in fixture["clamps"].as_array().unwrap() {
            let supported: Vec<String> = row["supported"]
                .as_array()
                .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                .unwrap_or_default();
            assert_eq!(
                serde_json::to_value(clamp_effort(
                    row["effort"].as_str(),
                    &supported,
                    row["overrides"].as_object()
                ))
                .unwrap(),
                row["result"],
                "{row}"
            );
        }
        for row in fixture["resolutions"].as_array().unwrap() {
            assert_eq!(
                resolve_config(&row["config"], row["model"].as_str().unwrap())
                    .unwrap_or(Value::Null),
                row["result"],
                "{row}"
            );
        }
    }
}
