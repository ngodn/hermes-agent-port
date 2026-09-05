//! Argument normalization before JSON validation in the conversation loop.
use serde_json::Value;

/// Python accepts dict/list tool arguments from provider adapters, serializes
/// them, and treats absent or whitespace-only text as an empty object. Other
/// scalars use str(value); malformed strings remain malformed for validation.
pub fn normalize(raw: &Value) -> String {
    match raw {
        Value::Object(_) | Value::Array(_) => dumps(raw),
        Value::Null => "{}".into(),
        Value::String(text)
            if text
                .trim_matches(crate::python_value::python_whitespace)
                .is_empty() =>
        {
            "{}".into()
        }
        Value::String(text) => text.clone(),
        value => crate::python_value::python_repr(value),
    }
}

/// json.dumps defaults: insertion order, spaced separators, ASCII escaping.
/// JSON Value excludes Python's non-finite numbers and lone surrogate strings.
fn dumps(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => crate::python_value::python_number(number),
        Value::String(text) => {
            let encoded = serde_json::to_string(text).expect("string serialization");
            let mut output = String::new();
            for character in encoded.chars() {
                if character >= '\u{7f}' {
                    use std::fmt::Write;
                    for unit in character.encode_utf16(&mut [0; 2]) {
                        write!(&mut output, "\\u{unit:04x}").expect("string formatting");
                    }
                } else {
                    output.push(character);
                }
            }
            output
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(dumps).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    dumps(&Value::String(key.clone())),
                    dumps(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Equality key for batch deduplication. Sorting a copy preserves original
/// argument spelling in replay while equivalent JSON objects share a key.
pub fn signature(raw: &str) -> String {
    fn sorted(value: Value) -> Value {
        match value {
            Value::Object(object) => {
                let ordered: std::collections::BTreeMap<_, _> = object
                    .into_iter()
                    .map(|(key, value)| (key, sorted(value)))
                    .collect();
                Value::Object(ordered.into_iter().collect())
            }
            Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
            value => value,
        }
    }
    serde_json::from_str::<Value>(raw)
        .map(|value| sorted(value).to_string())
        .unwrap_or_else(|_| raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_normalization_cases() {
        let rows: Value = serde_json::from_str(include_str!(
            "../../../tools/tool-argument-normalization-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            assert_eq!(
                normalize(&row["raw"]),
                row["expected"].as_str().unwrap(),
                "{row}"
            );
        }
    }
}
