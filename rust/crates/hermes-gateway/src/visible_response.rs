//! Visible assistant text used to recognize and recover post-tool answers.
//!
//! The ordered passes mirror `agent_runtime_helpers.strip_think_blocks` for
//! string content. Keep this separate from iteration-summary cleanup, whose
//! Python path intentionally uses a narrower, case-sensitive expression.
use fancy_regex::Regex;
use std::sync::LazyLock;

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let reasoning = [
        "think",
        "thinking",
        "reasoning",
        "REASONING_SCRATCHPAD",
        "thought",
    ];
    let tools = [
        "tool_call",
        "tool_calls",
        "tool_result",
        "function_call",
        "function_calls",
    ];
    let mut patterns: Vec<String> = reasoning
        .iter()
        .map(|name| format!(r"(?si)<{name}>.*?</{name}>"))
        .collect();
    patterns.extend(
        tools
            .iter()
            .map(|name| format!(r"(?si)<{name}\b[^>]*>.*?</{name}>")),
    );
    patterns.push(r"(?si)(?:(?<=^)|(?<=[\n\r.!?:]))[ \t]*<function\b[^>]*\bname\s*=[^>]*>(?:(?:(?!</function>).)*)</function>".into());
    let reasoning = reasoning.join("|");
    let tools = tools.join("|");
    patterns.push(format!(r"(?si)(?:^|\n)[ \t]*<(?:{reasoning})\b[^>]*>.*$"));
    patterns.push(format!(r"(?i)</?(?:{reasoning})>\s*"));
    patterns.push(format!(r"(?i)</(?:{tools}|function)>\s*"));
    patterns.push(format!(
        r"(?si)(?:^|\n)[ \t]*<(?:{tools})\b[^>]*>.*$|(?:^|\n)[^\n<]*</?arg_(?:key|value)\b.*$"
    ));
    patterns
        .into_iter()
        .map(|pattern| {
            // Python includes the four C0 information separators in whitespace.
            // Python IGNORECASE also equates dotted and dotless I. Rust's
            // Unicode case folding does not, so expand literal tag letters.
            let (flags, body) = pattern.split_once(')').expect("pattern flags");
            let body = body.replace(['i', 'I'], "[iİı]");
            let pattern = format!("{flags}){body}");
            Regex::new(&pattern.replace(r"\s", r"[\s\x1c-\x1f]"))
                .expect("fixed visible-text pattern")
        })
        .collect()
});

/// Strip protocol and reasoning markup without trimming ordinary visible text.
pub fn strip(text: &str) -> String {
    PATTERNS.iter().fold(text.to_owned(), |content, pattern| {
        pattern.replace_all(&content, "").into_owned()
    })
}

/// Return a normalized answer only when the model supplied visible content.
pub fn answer(text: &str) -> Option<String> {
    let cleaned = strip(text);
    let cleaned = cleaned.trim_matches(crate::python_value::python_whitespace);
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn matches_python_visible_text() {
        let rows: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/visible-response-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            assert_eq!(
                super::strip(row["input"].as_str().unwrap()),
                row["expected"].as_str().unwrap(),
                "{row}"
            );
        }
    }
}
