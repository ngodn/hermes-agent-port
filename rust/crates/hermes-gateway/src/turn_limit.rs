//! Turn-limit configuration shared by native agent construction.
//!
//! Python uses sys.maxsize as the unlimited sentinel. A finite Rust loop uses
//! the same platform-sized bound. Values beyond the counter's range fail at
//! construction instead of silently wrapping to a small limit.
use hermes_core::{Error, Result};
use serde_json::Value;

pub const UNLIMITED: usize = isize::MAX as usize;

/// Resolve agent.max_turns with the gateway's config-over-environment authority.
/// An explicit null clears the environment fallback; an absent key preserves it.
pub fn gateway(config: &Value, environment: Option<&str>) -> Result<usize> {
    match config.get("agent").and_then(|agent| agent.get("max_turns")) {
        Some(raw) => resolve(raw, UNLIMITED),
        None => resolve(
            &environment.map(Value::from).unwrap_or(Value::Null),
            UNLIMITED,
        ),
    }
}

/// Port of hermes_cli.config.resolve_turn_limit for JSON configuration values.
/// Invalid types and text use the caller's default; nonpositive values remove
/// the cap. Infinite numeric text raises an error in the Python reference too.
pub fn resolve(raw: &Value, default: usize) -> Result<usize> {
    let text = match raw {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text
            .trim_matches(crate::python_value::python_whitespace)
            .to_lowercase(),
        _ => return Ok(default),
    };
    if text.is_empty() {
        return Ok(default);
    }
    if matches!(
        text.as_str(),
        "none" | "null" | "unlimited" | "infinite" | "infinity" | "inf" | "∞" | "-1" | "0"
    ) {
        return Ok(UNLIMITED);
    }
    let Some(text) = crate::python_value::numeric_text(&text) else {
        return Ok(default);
    };
    if let Ok(integer) = text.parse::<i128>() {
        return if integer <= 0 {
            Ok(UNLIMITED)
        } else {
            usize::try_from(integer).map_err(|_| out_of_range())
        };
    }
    let digits = text.strip_prefix(['+', '-']).unwrap_or(&text);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        // An arbitrarily large negative Python integer still means unlimited.
        return if text.starts_with('-') {
            Ok(UNLIMITED)
        } else {
            Err(out_of_range())
        };
    }
    // int(string) falls back to int(float(string)), including decimal and
    // exponent spellings. Reject characters Python's float parser rejects.
    if !text
        .chars()
        .all(|c| c.is_ascii_digit() || "+-.eEinfatyINFATY".contains(c))
    {
        return Ok(default);
    }
    let Ok(number) = text.parse::<f64>() else {
        return Ok(default);
    };
    if number.is_nan() {
        return Ok(default);
    }
    if !number.is_finite() {
        return Err(out_of_range());
    }
    let number = number.trunc();
    if number <= 0.0 {
        Ok(UNLIMITED)
    } else if number < usize::MAX as f64 {
        Ok(number as usize)
    } else {
        Err(out_of_range())
    }
}

fn out_of_range() -> Error {
    Error::Other("agent.max_turns exceeds the native iteration counter range".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_reference_cases() {
        let rows: Value =
            serde_json::from_str(include_str!("../../../tools/turn-limit-goldens.json")).unwrap();
        for row in rows.as_array().unwrap() {
            let actual = resolve(&row["raw"], row["default"].as_u64().unwrap() as usize);
            if row.get("error").is_some() {
                assert!(actual.is_err(), "{row}");
            } else {
                assert_eq!(
                    actual.unwrap() as u64,
                    row["expected"].as_u64().unwrap(),
                    "{row}"
                );
            }
        }
    }

    #[test]
    fn config_controls_environment_authority() {
        assert_eq!(gateway(&json!({}), Some("12")).unwrap(), 12);
        assert_eq!(
            gateway(&json!({"agent": {"max_turns": 3}}), Some("12")).unwrap(),
            3
        );
        assert_eq!(
            gateway(&json!({"agent": {"max_turns": null}}), Some("12")).unwrap(),
            UNLIMITED
        );
        assert_eq!(
            gateway(&json!({"agent": {"max_turns": false}}), Some("12")).unwrap(),
            UNLIMITED
        );
    }
}
