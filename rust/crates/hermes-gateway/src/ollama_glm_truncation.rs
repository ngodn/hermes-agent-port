//! Conservative correction for local Ollama GLM stop-reason misreports.
//!
//! Some local Ollama GLM responses stop after tool use while claiming a normal
//! `stop`. This classifier changes only that narrow case into the existing
//! length-continuation path. Hosted Ollama and unrelated local servers remain
//! untouched.

use serde_json::Value;

pub struct Candidate<'a> {
    pub finish_reason: Option<&'a str>,
    pub api_mode: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub base_url: &'a str,
    pub messages: &'a [Value],
    pub assistant_message: Option<&'a Value>,
}

pub fn should_rewrite(candidate: Candidate<'_>) -> bool {
    if candidate.finish_reason != Some("stop") || candidate.api_mode != "chat_completions" {
        return false;
    }
    if !is_local_ollama_glm(candidate.provider, candidate.model, candidate.base_url) {
        return false;
    }
    if !candidate.messages.iter().any(|message| {
        message
            .as_object()
            .is_some_and(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
    }) {
        return false;
    }

    let Some(assistant) = candidate.assistant_message else {
        return false;
    };
    if assistant
        .get("tool_calls")
        .is_some_and(crate::python_value::truthy)
    {
        return false;
    }
    let Some(content) = assistant.get("content").and_then(Value::as_str) else {
        return false;
    };
    let visible = crate::visible_response::strip(content);
    let visible = visible.trim_matches(crate::python_value::python_whitespace);
    if visible.is_empty()
        || visible.chars().count() < 20
        || !visible.chars().any(crate::python_value::python_whitespace)
    {
        return false;
    }
    !has_natural_ending(visible)
}

fn is_local_ollama_glm(provider: &str, model: &str, base_url: &str) -> bool {
    let provider = provider.to_lowercase();
    let model = model.to_lowercase();
    if !model.contains("glm") && provider != "zai" {
        return false;
    }
    let base_url = base_url.to_lowercase();
    if base_url.contains("ollama.com") || model.contains(":cloud") {
        return false;
    }
    base_url.contains("ollama") || base_url.contains(":11434") || provider == "ollama"
}

fn has_natural_ending(content: &str) -> bool {
    let content = content.trim_end_matches(crate::python_value::python_whitespace);
    if content.is_empty() {
        return false;
    }
    if content.ends_with("```") {
        return true;
    }
    content.chars().last().is_some_and(|last| {
        matches!(
            last,
            '.' | '!'
                | '?'
                | ':'
                | ')'
                | '"'
                | '\''
                | ']'
                | '}'
                | '。'
                | '！'
                | '？'
                | '：'
                | '）'
                | '】'
                | '」'
                | '』'
                | '》'
                | '^'
        ) || u32::from(last) >= 0x1F300
    })
}

#[cfg(test)]
mod tests {
    use super::{should_rewrite, Candidate};
    use serde_json::{json, Value};

    fn goldens() -> Value {
        serde_json::from_str(include_str!(
            "../../../tools/ollama-glm-truncation-goldens.json"
        ))
        .unwrap()
    }

    #[test]
    fn classifier_matches_all_source_executed_python_goldens() {
        let fixture = goldens();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 113);
        assert_eq!(fixture["metadata"]["positive_cases_count"], 43);
        assert_eq!(fixture["metadata"]["negative_cases_count"], 70);

        for row in cases {
            let inputs = &row["inputs"];
            let messages = inputs["messages"].as_array().cloned().unwrap_or_default();
            let assistant = row["intermediate_gates"]["assistant_is_not_none"]
                .as_bool()
                .unwrap()
                .then(|| {
                    json!({
                        "role":"assistant",
                        "content":inputs["assistant_content"],
                        "tool_calls":inputs["assistant_tool_calls"],
                    })
                });
            let expected = row["outputs"]["should_treat_stop_as_truncated"]
                .as_bool()
                .unwrap();

            assert_eq!(
                should_rewrite(Candidate {
                    finish_reason: inputs["finish_reason"].as_str(),
                    api_mode: inputs["api_mode"].as_str().unwrap(),
                    provider: inputs["provider"].as_str().unwrap(),
                    model: inputs["model"].as_str().unwrap(),
                    base_url: inputs["base_url"].as_str().unwrap(),
                    messages: &messages,
                    assistant_message: assistant.as_ref(),
                }),
                expected,
                "{}",
                row["case_id"]
            );
        }
    }
}
