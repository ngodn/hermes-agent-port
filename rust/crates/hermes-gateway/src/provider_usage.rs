//! Canonical provider usage for native model calls.
//!
//! This is the JSON counterpart of `agent.usage_pricing.normalize_usage`.
//! Native clients currently use Chat Completions, but the other two modes are
//! kept here because auxiliary and Codex transports share the same accounting
//! contract.

use std::ops::{Add, AddAssign};

use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApiMode {
    #[default]
    ChatCompletions,
    AnthropicMessages,
    CodexResponses,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub request_count: u64,
}

impl Default for CanonicalUsage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            request_count: 1,
        }
    }
}

impl CanonicalUsage {
    pub fn accumulator() -> Self {
        Self {
            request_count: 0,
            ..Self::default()
        }
    }

    pub(crate) fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    #[cfg(test)]
    fn total_tokens(&self) -> u64 {
        self.prompt_tokens().saturating_add(self.output_tokens)
    }
}

impl AddAssign<&CanonicalUsage> for CanonicalUsage {
    fn add_assign(&mut self, rhs: &CanonicalUsage) {
        self.input_tokens = self.input_tokens.saturating_add(rhs.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(rhs.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(rhs.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(rhs.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(rhs.reasoning_tokens);
        self.request_count = self.request_count.saturating_add(rhs.request_count);
    }
}

impl Add for CanonicalUsage {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self::Output {
        self += &rhs;
        self
    }
}

/// Match Python's `max(0, int(value or 0))` for JSON scalar values, bounded to
/// the native counter width.
fn count(value: Option<&Value>) -> u64 {
    let Some(value) = value else {
        return 0;
    };
    match value {
        Value::Null => 0,
        Value::Bool(value) => u64::from(*value),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                return u64::try_from(value).unwrap_or(0);
            }
            if let Some(value) = value.as_u64() {
                return value;
            }
            value.as_f64().map_or(0, |value| {
                if !value.is_finite() || value <= 0.0 {
                    0
                } else if value >= u64::MAX as f64 {
                    u64::MAX
                } else {
                    value.trunc() as u64
                }
            })
        }
        Value::String(value) => {
            let normalized = value.trim().replace('_', "");
            normalized.parse::<i128>().map_or(0, |value| {
                u64::try_from(value).unwrap_or(if value < 0 { 0 } else { u64::MAX })
            })
        }
        Value::Array(_) | Value::Object(_) => 0,
    }
}

fn first_nonzero<'a>(values: impl IntoIterator<Item = Option<&'a Value>>) -> u64 {
    values
        .into_iter()
        .map(count)
        .find(|value| *value != 0)
        .unwrap_or(0)
}

pub fn normalize_usage(usage: &Value, mode: ApiMode, provider: Option<&str>) -> CanonicalUsage {
    let Some(usage) = usage.as_object() else {
        return CanonicalUsage::default();
    };
    let anthropic = mode == ApiMode::AnthropicMessages
        || provider.is_some_and(|provider| provider.trim().eq_ignore_ascii_case("anthropic"));

    let (input_tokens, output_tokens, cache_read_tokens, cache_write_tokens) = if anthropic {
        (
            count(usage.get("input_tokens")),
            count(usage.get("output_tokens")),
            count(usage.get("cache_read_input_tokens")),
            count(usage.get("cache_creation_input_tokens")),
        )
    } else if mode == ApiMode::CodexResponses {
        let details = usage.get("input_tokens_details");
        let cache_read = count(details.and_then(|value| value.get("cached_tokens")));
        let cache_write = first_nonzero([
            details.and_then(|value| value.get("cache_write_tokens")),
            details.and_then(|value| value.get("cache_creation_tokens")),
        ]);
        let total = count(usage.get("input_tokens"));
        (
            total.saturating_sub(cache_read.saturating_add(cache_write)),
            count(usage.get("output_tokens")),
            cache_read,
            cache_write,
        )
    } else {
        let details = usage.get("prompt_tokens_details");
        let cache_read = first_nonzero([
            details.and_then(|value| value.get("cached_tokens")),
            usage.get("cache_read_input_tokens"),
            usage.get("prompt_cache_hit_tokens"),
            usage.get("cached_tokens"),
        ]);
        let cache_write = first_nonzero([
            details.and_then(|value| value.get("cache_write_tokens")),
            details.and_then(|value| value.get("cache_creation_input_tokens")),
            usage.get("cache_creation_input_tokens"),
            usage.get("cache_write_tokens"),
        ]);
        let total = first_nonzero([usage.get("prompt_tokens"), usage.get("input_tokens")]);
        (
            total.saturating_sub(cache_read.saturating_add(cache_write)),
            first_nonzero([usage.get("completion_tokens"), usage.get("output_tokens")]),
            cache_read,
            cache_write,
        )
    };

    let reasoning_tokens = first_nonzero([
        usage
            .get("output_tokens_details")
            .and_then(|value| value.get("reasoning_tokens")),
        usage
            .get("completion_tokens_details")
            .and_then(|value| value.get("reasoning_tokens")),
    ]);
    CanonicalUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        request_count: 1,
    }
}

pub fn from_response(
    response: &Value,
    mode: ApiMode,
    provider: Option<&str>,
) -> Option<CanonicalUsage> {
    let usage = response.get("usage")?;
    (!usage.is_null()).then(|| normalize_usage(usage, mode, provider))
}

pub fn from_sse_line(line: &str, mode: ApiMode, provider: Option<&str>) -> Option<CanonicalUsage> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    let payload = line.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    let response: Value = serde_json::from_str(payload).ok()?;
    from_response(&response, mode, provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_usage_matches_python_cache_fallbacks() {
        let usage = normalize_usage(
            &json!({
                "prompt_tokens": 120,
                "completion_tokens": 13,
                "prompt_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 5},
                "cache_read_input_tokens": 90,
                "completion_tokens_details": {"reasoning_tokens": 7}
            }),
            ApiMode::ChatCompletions,
            None,
        );
        assert_eq!(
            usage,
            CanonicalUsage {
                input_tokens: 75,
                output_tokens: 13,
                cache_read_tokens: 40,
                cache_write_tokens: 5,
                reasoning_tokens: 7,
                request_count: 1,
            }
        );

        let deepseek = normalize_usage(
            &json!({"prompt_tokens": 100, "prompt_cache_hit_tokens": 60}),
            ApiMode::ChatCompletions,
            None,
        );
        assert_eq!(deepseek.input_tokens, 40);
        assert_eq!(deepseek.cache_read_tokens, 60);

        let kimi = normalize_usage(
            &json!({"prompt_tokens": 100, "cached_tokens": 25}),
            ApiMode::ChatCompletions,
            None,
        );
        assert_eq!(kimi.input_tokens, 75);
        assert_eq!(kimi.cache_read_tokens, 25);
    }

    #[test]
    fn native_modes_keep_python_input_semantics() {
        let anthropic = normalize_usage(
            &json!({
                "input_tokens": 10,
                "output_tokens": 4,
                "cache_read_input_tokens": 20,
                "cache_creation_input_tokens": 3
            }),
            ApiMode::AnthropicMessages,
            None,
        );
        assert_eq!(anthropic.input_tokens, 10);
        assert_eq!(anthropic.prompt_tokens(), 33);

        let codex = normalize_usage(
            &json!({
                "input_tokens": 80,
                "output_tokens": 9,
                "input_tokens_details": {"cached_tokens": 30, "cache_creation_tokens": 2},
                "output_tokens_details": {"reasoning_tokens": 6}
            }),
            ApiMode::CodexResponses,
            None,
        );
        assert_eq!(codex.input_tokens, 48);
        assert_eq!(codex.total_tokens(), 89);
        assert_eq!(codex.reasoning_tokens, 6);
    }

    #[test]
    fn counters_follow_python_coercion_and_clamping() {
        assert_eq!(count(Some(&json!(true))), 1);
        assert_eq!(count(Some(&json!(12.9))), 12);
        assert_eq!(count(Some(&json!(" 1_024 "))), 1024);
        assert_eq!(count(Some(&json!(-4))), 0);
        assert_eq!(count(Some(&json!("3.5"))), 0);
        assert_eq!(count(Some(&json!({}))), 0);
    }

    #[test]
    fn response_and_sse_extract_only_explicit_usage() {
        let response = json!({"usage":{"prompt_tokens":5,"completion_tokens":2}});
        assert_eq!(
            from_response(&response, ApiMode::ChatCompletions, None)
                .unwrap()
                .total_tokens(),
            7
        );
        let line = format!("data: {response}\r\n");
        assert_eq!(
            from_sse_line(&line, ApiMode::ChatCompletions, None)
                .unwrap()
                .prompt_tokens(),
            5
        );
        assert!(from_sse_line("data: [DONE]", ApiMode::ChatCompletions, None).is_none());
        assert!(from_sse_line(
            r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
            ApiMode::ChatCompletions,
            None
        )
        .is_none());
    }

    #[test]
    fn addition_is_saturating_and_counts_requests() {
        let mut total = CanonicalUsage::accumulator();
        total += &CanonicalUsage {
            input_tokens: u64::MAX,
            ..CanonicalUsage::default()
        };
        total += &CanonicalUsage {
            input_tokens: 1,
            ..CanonicalUsage::default()
        };
        assert_eq!(total.input_tokens, u64::MAX);
        assert_eq!(total.request_count, 2);
    }
}
