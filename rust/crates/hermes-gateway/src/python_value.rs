//! Python scalar coercions shared by configuration and catalog ports.
use serde_json::{json, Value};

pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

pub(crate) fn integer(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(value) => Some(json!(i64::from(*value))),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(value.clone()),
        Value::Number(number) => {
            let number = number.as_f64()?.trunc();
            // Never silently saturate oversized catalog sizes. JSON's integer
            // representation is bounded, unlike Python's arbitrary precision.
            if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&number) {
                Some(json!(number as i64))
            } else if (0.0..18_446_744_073_709_551_616.0).contains(&number) {
                Some(json!(number as u64))
            } else {
                None
            }
        }
        Value::String(value) => {
            let number = numeric_text(value)?;
            number
                .parse::<i64>()
                .map(|n| json!(n))
                .ok()
                .or_else(|| number.parse::<u64>().map(|n| json!(n)).ok())
        }
        _ => None,
    }
}

/// Python accepts Unicode decimal digits and underscores between digits.
pub(crate) fn numeric_text(value: &str) -> Option<String> {
    let chars: Vec<char> = value.trim().chars().collect();
    let mut result = String::new();
    for (index, c) in chars.iter().copied().enumerate() {
        if c == '_' {
            if index == 0
                || index + 1 == chars.len()
                || decimal_digit(chars[index - 1]).is_none()
                || decimal_digit(chars[index + 1]).is_none()
            {
                return None;
            }
        } else if let Some(digit) = decimal_digit(c) {
            result.push(char::from(b'0' + digit as u8));
        } else {
            result.push(c);
        }
    }
    Some(result)
}

// Python prints small/large floats in scientific notation with an explicit
// exponent sign and at least two digits. Preserve those coerced provider keys.
pub(crate) fn python_number(value: &serde_json::Number) -> String {
    if !value.is_f64() {
        return value.to_string();
    }
    let number = value.as_f64().unwrap();
    let rendered = if number != 0.0 && !(1e-4..1e16).contains(&number.abs()) {
        format!("{number:e}")
    } else {
        value.to_string()
    };
    if let Some((mantissa, exponent)) = rendered.split_once('e') {
        let exponent: i32 = exponent.parse().expect("finite JSON float exponent");
        format!("{mantissa}e{exponent:+03}")
    } else {
        rendered
    }
}

pub(crate) fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(value) => if *value { "True" } else { "False" }.into(),
        Value::Number(value) => python_number(value),
        Value::String(value) => {
            use unicode_general_category::{get_general_category, GeneralCategory::*};
            let quote = if value.contains('\'') && !value.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut result = String::from(quote);
            for c in value.chars() {
                match c {
                    '\\' => result.push_str("\\\\"),
                    '\n' => result.push_str("\\n"),
                    '\r' => result.push_str("\\r"),
                    '\t' => result.push_str("\\t"),
                    c if c == quote => {
                        result.push('\\');
                        result.push(c);
                    }
                    c if c != ' '
                        && matches!(
                            get_general_category(c),
                            Control
                                | Format
                                | Surrogate
                                | PrivateUse
                                | Unassigned
                                | SpaceSeparator
                                | LineSeparator
                                | ParagraphSeparator
                        ) =>
                    {
                        let code = c as u32;
                        result.push_str(&if code <= 0xff {
                            format!("\\x{code:02x}")
                        } else if code <= 0xffff {
                            format!("\\u{code:04x}")
                        } else {
                            format!("\\U{code:08x}")
                        });
                    }
                    c => result.push(c),
                }
            }
            result.push(quote);
            result
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    python_repr(&Value::String(key.clone())),
                    python_repr(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

// Unicode 15 decimal-digit blocks used by CPython 3.12. All blocks contain
// ten consecutive digits; the oracle covers every alphabet's private prefix.
pub(crate) fn decimal_digit(c: char) -> Option<i64> {
    const ZEROES: &[u32] = &[
        0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
        0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
        0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
        0xff10, 0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0,
        0x11650, 0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60,
        0x16ac0, 0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0,
        0x1e950, 0x1fbf0,
    ];
    ZEROES.iter().find_map(|zero| {
        (c as u32)
            .checked_sub(*zero)
            .filter(|digit| *digit < 10)
            .map(i64::from)
    })
}

/// CPython str.strip includes four information separators beyond Unicode whitespace.
pub(crate) fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
