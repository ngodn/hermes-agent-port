//! Content guards for ordinary main-provider length truncations.

use fancy_regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

const MIN_FRAGMENT_LENGTH: usize = 400;
const REPEAT_WINDOW: usize = 60;
const MIN_REPEAT_COUNT: usize = 5;

static THINKING_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(?:think|thinking|reasoning|REASONING_SCRATCHPAD)[^>]*>")
        .expect("fixed thinking tag pattern")
});

pub const THINKING_EXHAUSTED_RESPONSE: &str = "⚠️ **Thinking Budget Exhausted**\n\nThe model used all its output tokens on reasoning and had none left for the actual response.\n\nTo fix this:\n→ Lower reasoning effort: `/reasoning low` or `/reasoning minimal`\n→ Or switch to a larger/non-reasoning model with `/model`";
pub const REPETITION_RESPONSE: &str = "⚠️ **Response Stopped -- Repetition Detected**\n\nThe model fell into a repetition loop while writing this response, so continuing would only produce more repeated text. The partial response was discarded.\n\n→ Switch to a different model with `/model`\n→ Or resend your message (your conversation history is preserved)";
pub const NO_VISIBLE_RESPONSE: &str = "⚠️ **No visible answer was produced.** The model hit its output-token limit on every continuation attempt -- its reasoning consumed the entire budget each time.\n\nTo fix this:\n→ Lower reasoning effort: `/reasoning low` or `/reasoning none`\n→ Or raise max_tokens for this model";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardDisposition {
    Continue { disable_reasoning_once: bool },
    AbortThinkingExhausted,
    AbortRepetitionDominated,
}

impl GuardDisposition {
    pub fn terminal_response(self) -> Option<&'static str> {
        match self {
            Self::Continue { .. } => None,
            Self::AbortThinkingExhausted => Some(THINKING_EXHAUSTED_RESPONSE),
            Self::AbortRepetitionDominated => Some(REPETITION_RESPONSE),
        }
    }

    pub fn disable_reasoning_once(self) -> bool {
        matches!(
            self,
            Self::Continue {
                disable_reasoning_once: true
            }
        )
    }
}

pub fn classify(content: Option<&str>, has_tool_calls: bool) -> GuardDisposition {
    let thinking_exhausted = !has_tool_calls
        && content.is_some_and(|content| {
            THINKING_TAG.is_match(content).unwrap_or(false)
                && crate::visible_response::answer(content).is_none()
        });
    if thinking_exhausted {
        return GuardDisposition::AbortThinkingExhausted;
    }
    if !has_tool_calls
        && content.is_some_and(|content| {
            let visible = crate::visible_response::strip(content);
            !visible.is_empty() && is_repetition_dominated(&visible)
        })
    {
        return GuardDisposition::AbortRepetitionDominated;
    }
    GuardDisposition::Continue {
        disable_reasoning_once: !has_tool_calls && content.is_none_or(str::is_empty),
    }
}

pub fn reasoning_disabled_once(config: Option<&serde_json::Value>) -> serde_json::Value {
    let mut disabled = config
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    disabled.insert("enabled".into(), serde_json::Value::Bool(false));
    disabled.insert("effort".into(), serde_json::Value::String("none".into()));
    serde_json::Value::Object(disabled)
}

pub fn reasoning_is_disabled(config: &serde_json::Value) -> bool {
    config.as_object().is_some_and(|config| {
        config.get("enabled") == Some(&serde_json::Value::Bool(false))
            || config
                .get("effort")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|effort| effort.eq_ignore_ascii_case("none"))
    })
}

pub fn append_reasoning_only_nudge(messages: &mut Vec<serde_json::Value>, prompt: &str) {
    if let Some(previous) = messages
        .last_mut()
        .filter(|message| message["role"] == "user")
    {
        if let Some(content) = previous["content"].as_str() {
            let separator = if content.is_empty() { "" } else { "\n\n" };
            previous["content"] =
                serde_json::Value::String(format!("{content}{separator}{prompt}"));
            return;
        }
    }
    messages.push(serde_json::json!({
        "role": "user",
        "content": prompt,
        "_length_continuation_reasoning_only": true,
    }));
}

fn is_repetition_dominated(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let length = chars.len();
    if length < MIN_FRAGMENT_LENGTH {
        return false;
    }

    let mut lines = HashMap::<&str, usize>::new();
    for line in text.split(|character| {
        matches!(
            character,
            '\n' | '\r'
                | '\u{000b}'
                | '\u{000c}'
                | '\u{001c}'
                | '\u{001d}'
                | '\u{001e}'
                | '\u{0085}'
                | '\u{2028}'
                | '\u{2029}'
        )
    }) {
        let line = line.trim_matches(crate::python_value::python_whitespace);
        if !line.is_empty() {
            *lines.entry(line).or_default() += 1;
        }
    }
    if lines.into_iter().any(|(line, count)| {
        count >= MIN_REPEAT_COUNT
            && count.saturating_mul(line.chars().count()).saturating_mul(2) >= length
    }) {
        return true;
    }

    let needed = MIN_REPEAT_COUNT.max(length.div_ceil(REPEAT_WINDOW * 2));
    let mut windows = HashMap::<String, usize>::new();
    for start in 0..=length - REPEAT_WINDOW {
        let window: String = chars[start..start + REPEAT_WINDOW].iter().collect();
        let count = windows.entry(window).or_default();
        *count += 1;
        if *count >= needed {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{classify, is_repetition_dominated, GuardDisposition};

    fn goldens() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tools/main-provider-truncation-guard-goldens.json"
        ))
        .unwrap()
    }

    fn expected_repetition(name: &str) -> bool {
        goldens()["repetition_dominated_rejection"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["case_name"] == name)
            .and_then(|row| row["is_repetition_dominated"].as_bool())
            .unwrap()
    }

    #[test]
    fn thinking_classification_matches_python_goldens() {
        for row in goldens()["thinking_exhausted_detection"]
            .as_array()
            .unwrap()
        {
            let Some(content) = row.get("content_snippet") else {
                continue;
            };
            let content = if content.is_null() {
                None
            } else {
                Some(content.as_str().unwrap())
            };
            let has_tool_calls = row["has_tool_calls"].as_bool().unwrap();
            let expected = row["thinking_exhausted"].as_bool().unwrap();
            assert_eq!(
                classify(content, has_tool_calls) == GuardDisposition::AbortThinkingExhausted,
                expected,
                "{}",
                row["case_name"]
            );
        }
    }

    #[test]
    fn repetition_classification_matches_python_goldens() {
        let incident = "好，你幫我更改成 Google Gemini 4 31B。";
        let short = incident.repeat(5);
        assert_eq!(
            is_repetition_dominated(&short),
            expected_repetition("below_min_fragment_length_fail_open")
        );

        let mut boundary = ("A".repeat(60) + " ").repeat(6);
        boundary.push_str(&"X".repeat(399 - boundary.chars().count()));
        assert_eq!(
            is_repetition_dominated(&boundary),
            expected_repetition("boundary_below_400_chars")
        );

        let incident_shape =
            format!("We need to verify the model setting.\n{incident}\n").repeat(800);
        assert_eq!(
            is_repetition_dominated(&incident_shape),
            expected_repetition("line_path_incident_shape_with_narration")
        );
        assert_eq!(
            is_repetition_dominated(&(incident.to_owned() + "\n").repeat(50)),
            expected_repetition("line_path_single_line_echo")
        );

        let minimum_line = ("Z".repeat(80) + "\n").repeat(5) + &"Filler unique text ".repeat(15);
        assert_eq!(
            is_repetition_dominated(&minimum_line),
            expected_repetition("line_path_minimal_count_and_dominance")
        );
        let filler = (0..2500)
            .map(|index| format!("unique token {index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let minority = filler + &(format!("\n{incident}\n").repeat(30));
        assert_eq!(
            is_repetition_dominated(&minority),
            expected_repetition("line_path_dominance_under_50_percent")
        );
        assert_eq!(
            is_repetition_dominated(&incident.repeat(2000)),
            expected_repetition("window_path_no_line_breaks_incident_echo")
        );
        let pattern = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz01234567";
        assert_eq!(
            is_repetition_dominated(&pattern.repeat(30)),
            expected_repetition("window_path_exact_60_char_sliding_pattern")
        );

        let diverse = (0..1200)
            .map(|index| {
                format!(
                    "Sentence number {index} describes a distinct topic with unique words such as quasar-{index} and nebula-{index} to keep every window distinct."
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            is_repetition_dominated(&diverse),
            expected_repetition("long_diverse_prose_not_flagged")
        );
    }

    #[test]
    fn tool_calls_preempt_content_guards_and_empty_content_arms_one_shot() {
        let repeated = "A".repeat(600);
        assert_eq!(
            classify(Some(&repeated), true),
            GuardDisposition::Continue {
                disable_reasoning_once: false
            }
        );
        assert!(classify(None, false).disable_reasoning_once());
        assert!(classify(Some(""), false).disable_reasoning_once());
        assert!(!classify(Some("short visible fragment"), false).disable_reasoning_once());
    }
}
