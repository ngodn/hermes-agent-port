//! Frozen ordinary main-provider liveness policy.
//!
//! The user-facing authority is `config.yaml`. Existing environment values are
//! accepted only as compatibility fallbacks after provider and model settings.

use std::time::Duration;

use serde_json::Value;

const DEFAULT_REQUEST_TIMEOUT_SECONDS: f64 = 1_800.0;
const DEFAULT_STREAM_STALE_TIMEOUT_SECONDS: f64 = 180.0;
const DEFAULT_BUFFERED_STALE_TIMEOUT_SECONDS: f64 = 90.0;
const DEFAULT_LOCAL_STREAM_STALE_TIMEOUT_SECONDS: f64 = 900.0;
const DEFAULT_STREAM_READ_TIMEOUT_SECONDS: f64 = 120.0;
const DEFAULT_STREAM_RETRIES: usize = 2;
const DEFAULT_STALE_GIVEUP: usize = 5;
const MAX_SAFE_TIMEOUT_SECONDS: f64 = 31_536_000.0;

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    request_timeout: Duration,
    request_configured: bool,
    stream_stale_base: Duration,
    stream_read_base: Duration,
    buffered_stale_base: Duration,
    buffered_stale_implicit: bool,
    buffered_stale_explicit: bool,
    local_stream_stale: Duration,
    stream_attempts: usize,
    stale_giveup: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs_f64(DEFAULT_REQUEST_TIMEOUT_SECONDS),
            request_configured: false,
            stream_stale_base: Duration::from_secs_f64(DEFAULT_STREAM_STALE_TIMEOUT_SECONDS),
            stream_read_base: Duration::from_secs_f64(DEFAULT_STREAM_READ_TIMEOUT_SECONDS),
            buffered_stale_base: Duration::from_secs_f64(DEFAULT_BUFFERED_STALE_TIMEOUT_SECONDS),
            buffered_stale_implicit: true,
            buffered_stale_explicit: false,
            local_stream_stale: Duration::from_secs_f64(DEFAULT_LOCAL_STREAM_STALE_TIMEOUT_SECONDS),
            stream_attempts: DEFAULT_STREAM_RETRIES + 1,
            stale_giveup: DEFAULT_STALE_GIVEUP,
        }
    }
}

fn positive_seconds(value: &Value) -> Option<f64> {
    let seconds = match value {
        Value::Bool(true) => 1.0,
        Value::Bool(false) | Value::Null => return None,
        Value::Number(value) => value.as_f64()?,
        Value::String(value) => value.trim().parse().ok()?,
        Value::Array(_) | Value::Object(_) => return None,
    };
    (seconds > 0.0).then_some(seconds.min(MAX_SAFE_TIMEOUT_SECONDS))
}

pub fn normalize_run_budget_seconds(value: &Value) -> Option<f64> {
    let seconds = match value {
        Value::Number(value) => value.as_f64()?,
        Value::String(value) => value.trim().parse().ok()?,
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => return None,
    };
    (!seconds.is_nan() && seconds > 0.0).then_some(seconds)
}

fn positive_environment_seconds(value: Option<&str>) -> Option<f64> {
    let seconds = value?.trim().parse::<f64>().ok()?;
    (seconds > 0.0).then_some(seconds.min(MAX_SAFE_TIMEOUT_SECONDS))
}

fn environment_integer(value: Option<&str>) -> Option<i128> {
    value?.trim().parse().ok()
}

fn provider_config<'a>(
    config: &'a Value,
    provider: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    config
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(provider))
        .and_then(Value::as_object)
}

fn configured_timeout(
    config: &Value,
    provider: &str,
    model: &str,
    model_key: &str,
    provider_key: &str,
) -> Option<f64> {
    let provider = provider_config(config, provider)?;
    provider
        .get("models")
        .and_then(Value::as_object)
        .and_then(|models| models.get(model))
        .and_then(Value::as_object)
        .and_then(|model| model.get(model_key))
        .and_then(positive_seconds)
        .or_else(|| provider.get(provider_key).and_then(positive_seconds))
}

impl Policy {
    /// Resolve Python's model, provider, environment, default precedence once
    /// at route construction. The effective inactivity deadline can still
    /// scale from the immutable request body without rereading live config.
    pub fn resolve(
        config: &Value,
        provider: &str,
        model: &str,
        mut environment: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        let configured_request = configured_timeout(
            config,
            provider,
            model,
            "timeout_seconds",
            "request_timeout_seconds",
        );
        let configured_stale = configured_timeout(
            config,
            provider,
            model,
            "stale_timeout_seconds",
            "stale_timeout_seconds",
        );
        let request_timeout = configured_request
            .or_else(|| positive_environment_seconds(environment("HERMES_API_TIMEOUT").as_deref()))
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECONDS);
        let stream_stale_base = configured_stale
            .or_else(|| {
                positive_environment_seconds(environment("HERMES_STREAM_STALE_TIMEOUT").as_deref())
            })
            .unwrap_or(DEFAULT_STREAM_STALE_TIMEOUT_SECONDS);
        let stream_read_base =
            positive_environment_seconds(environment("HERMES_STREAM_READ_TIMEOUT").as_deref())
                .unwrap_or(DEFAULT_STREAM_READ_TIMEOUT_SECONDS);
        let buffered_stale_environment =
            positive_environment_seconds(environment("HERMES_API_CALL_STALE_TIMEOUT").as_deref());
        let (buffered_stale_base, buffered_stale_implicit) = configured_stale
            .map(|seconds| (seconds, false))
            .or_else(|| buffered_stale_environment.map(|seconds| (seconds, false)))
            .or_else(|| reasoning_stale_timeout_floor(model).map(|seconds| (seconds, false)))
            .unwrap_or((DEFAULT_BUFFERED_STALE_TIMEOUT_SECONDS, true));
        let agent = config.get("agent").and_then(Value::as_object);
        let local_stream_stale = positive_environment_seconds(
            environment("HERMES_LOCAL_STREAM_STALE_TIMEOUT").as_deref(),
        )
        .or_else(|| {
            agent
                .and_then(|agent| agent.get("local_stream_stale_timeout"))
                .and_then(positive_seconds)
        })
        .unwrap_or(DEFAULT_LOCAL_STREAM_STALE_TIMEOUT_SECONDS);
        let stream_retries = environment_integer(environment("HERMES_STREAM_RETRIES").as_deref())
            .unwrap_or(DEFAULT_STREAM_RETRIES as i128)
            .max(0);
        let stale_giveup =
            environment_integer(environment("HERMES_STREAM_STALE_GIVEUP").as_deref())
                .unwrap_or(DEFAULT_STALE_GIVEUP as i128)
                .max(0);
        Self {
            request_timeout: Duration::from_secs_f64(request_timeout),
            request_configured: configured_request.is_some(),
            stream_stale_base: Duration::from_secs_f64(stream_stale_base),
            stream_read_base: Duration::from_secs_f64(stream_read_base),
            buffered_stale_base: Duration::from_secs_f64(buffered_stale_base),
            buffered_stale_implicit,
            buffered_stale_explicit: configured_stale.is_some()
                || buffered_stale_environment.is_some(),
            local_stream_stale: Duration::from_secs_f64(local_stream_stale),
            stream_attempts: usize::try_from(stream_retries)
                .unwrap_or(usize::MAX)
                .saturating_add(1),
            stale_giveup: usize::try_from(stale_giveup).unwrap_or(usize::MAX),
        }
    }

    pub fn request_timeout(self) -> Duration {
        self.request_timeout
    }

    pub fn stream_attempts(self) -> usize {
        self.stream_attempts
    }

    pub fn stale_giveup(self) -> usize {
        self.stale_giveup
    }

    pub fn stream_stale_timeout(self, base_url: &str, model: &str, body: &Value) -> Duration {
        if self.stream_stale_base.as_secs_f64() == DEFAULT_STREAM_STALE_TIMEOUT_SECONDS
            && crate::local_probe::is_local_endpoint(base_url)
        {
            return self.local_stream_stale;
        }
        let estimated = estimate_request_context_tokens(body);
        let context_floor = if estimated > 100_000 {
            300.0
        } else if estimated > 50_000 {
            240.0
        } else {
            0.0
        };
        let reasoning_floor = reasoning_stale_timeout_floor(model).unwrap_or(0.0);
        Duration::from_secs_f64(
            self.stream_stale_base
                .as_secs_f64()
                .max(context_floor)
                .max(reasoning_floor),
        )
    }

    /// Effective inactivity bound for one reqwest body read. Python's httpx
    /// socket timer can fire before the structured stale monitor when the user
    /// explicitly lowers its legacy read timeout or configures a short provider
    /// request timeout.
    pub fn stream_inactivity_timeout(self, base_url: &str, model: &str, body: &Value) -> Duration {
        let stale = self.stream_stale_timeout(base_url, model, body);
        let mut read = if self.request_configured {
            self.request_timeout
        } else {
            self.stream_read_base
        };
        if !self.request_configured && read.as_secs_f64() == DEFAULT_STREAM_READ_TIMEOUT_SECONDS {
            if crate::local_probe::is_local_endpoint(base_url) {
                read = self.request_timeout;
            } else if stale > read {
                read = stale;
            }
        }
        read.min(stale)
    }

    pub fn buffered_stale_timeout(self, base_url: &str, body: &Value) -> Option<Duration> {
        if self.buffered_stale_implicit && crate::local_probe::is_local_endpoint(base_url) {
            return None;
        }
        let estimated = estimate_request_context_tokens(body);
        let context_floor = if estimated > 100_000 {
            240.0
        } else if estimated > 50_000 {
            150.0
        } else {
            0.0
        };
        Some(Duration::from_secs_f64(
            self.buffered_stale_base.as_secs_f64().max(context_floor),
        ))
    }

    /// Apply Python's run-budget cap after local and context scaling. A run
    /// budget may shorten default and reasoning-floor timeouts, but explicit
    /// model, provider, or environment settings remain authoritative.
    pub fn buffered_stale_timeout_capped(
        self,
        base_url: &str,
        body: &Value,
        run_budget_remaining: Option<f64>,
    ) -> Option<Duration> {
        let timeout = self.buffered_stale_timeout(base_url, body)?;
        let Some(remaining) = run_budget_remaining else {
            return Some(timeout);
        };
        if self.buffered_stale_explicit {
            return Some(timeout);
        }
        let cap = (remaining * 0.5).max(60.0);
        if cap < timeout.as_secs_f64() {
            Some(Duration::from_secs_f64(cap))
        } else {
            Some(timeout)
        }
    }
}

pub fn estimate_request_context_tokens(payload: &Value) -> usize {
    fn chars(value: &Value) -> usize {
        if value.is_null() {
            return 0;
        }
        value.as_str().map_or_else(
            || crate::python_value::python_repr(value).chars().count(),
            |value| value.chars().count(),
        )
    }
    fn message_chars(value: &Value) -> usize {
        value
            .as_array()
            .map_or_else(|| chars(value), |items| items.iter().map(chars).sum())
    }

    match payload {
        Value::Array(_) => message_chars(payload) / 4,
        Value::Object(object) => {
            if object.get("messages").is_some_and(Value::is_array) {
                let total =
                    message_chars(&object["messages"]) + object.get("tools").map_or(0, chars);
                return total / 4;
            }
            if object.contains_key("input") {
                return ["input", "instructions", "tools"]
                    .iter()
                    .map(|key| object.get(*key).map_or(0, chars))
                    .sum::<usize>()
                    / 4;
            }
            object.values().map(chars).sum::<usize>() / 4
        }
        _ => chars(payload) / 4,
    }
}

pub fn reasoning_stale_timeout_floor(model: &str) -> Option<f64> {
    const FLOORS: &[(&str, f64)] = &[
        ("nemotron-3-ultra", 600.0),
        ("nemotron-3-super", 600.0),
        ("nemotron-3-nano", 300.0),
        ("nemotron-3.5-lightning", 300.0),
        ("deepseek-r1", 600.0),
        ("deepseek-reasoner", 600.0),
        ("deepseek-v4-flash", 600.0),
        ("deepseek-v4-pro", 600.0),
        ("qwq-32b", 300.0),
        ("qwen3", 180.0),
        ("o1", 600.0),
        ("o1-mini", 600.0),
        ("o1-pro", 600.0),
        ("o1-preview", 600.0),
        ("o3", 600.0),
        ("o3-pro", 600.0),
        ("o3-mini", 300.0),
        ("o4-mini", 300.0),
        ("claude-opus-4", 240.0),
        ("claude-opus-5", 240.0),
        ("claude-sonnet-5", 180.0),
        ("claude-sonnet-4.5", 180.0),
        ("claude-sonnet-4.6", 180.0),
        ("claude-fable", 600.0),
        ("grok-4-fast-reasoning", 300.0),
        ("grok-4.20-reasoning", 300.0),
        ("grok-4.5", 300.0),
        ("grok-4.6", 300.0),
        ("grok-4-fast-non-reasoning", 180.0),
        ("ox-alpha", 300.0),
        ("x-preview-f-free", 300.0),
        ("inkling", 300.0),
    ];
    let name = model.trim().to_lowercase();
    let slug = name.rsplit('/').next().unwrap_or(&name);
    FLOORS
        .iter()
        .filter(|(prefix, _)| {
            slug.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.is_empty()
                    || suffix
                        .chars()
                        .next()
                        .is_some_and(|next| matches!(next, '-' | '.' | '_' | ':'))
            })
        })
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, floor)| *floor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn goldens() -> Value {
        serde_json::from_str(include_str!(
            "../../../tools/main-provider-stall-goldens.json"
        ))
        .unwrap()
    }

    fn run_budget_goldens() -> Value {
        serde_json::from_str(include_str!(
            "../../../tools/main-provider-run-budget-goldens.json"
        ))
        .unwrap()
    }

    #[test]
    fn reasoning_floors_match_executed_python_goldens() {
        for case in goldens()["reasoning_model_floors"].as_array().unwrap() {
            let Some(model) = case["model_input"].as_str() else {
                continue;
            };
            assert_eq!(
                reasoning_stale_timeout_floor(model),
                case["expected_floor"].as_f64(),
                "{}",
                case["case_name"]
            );
        }
    }

    #[test]
    fn context_estimates_match_executed_python_goldens() {
        let expected = |name: &str| {
            goldens()["context_token_estimation"]
                .as_array()
                .unwrap()
                .iter()
                .find(|case| case["case_name"] == name)
                .unwrap()["estimated_tokens"]
                .as_u64()
                .unwrap() as usize
        };
        let bare = json!([
            {"role":"user", "content":"a".repeat(400)},
            {"role":"assistant", "content":"b".repeat(800)}
        ]);
        let chat = json!({
            "model":"gpt-4o",
            "messages":[{"role":"user", "content":"x".repeat(2000)}],
            "tools":[{"type":"function", "function":{
                "name":"test_fn", "description":"d".repeat(400)
            }}]
        });
        let responses = json!({
            "model":"gpt-5.5",
            "instructions":"i".repeat(1000),
            "input":"u".repeat(4000),
            "tools":[{"name":"t1", "description":"tool desc"}]
        });

        assert_eq!(estimate_request_context_tokens(&Value::Null), 0);
        assert_eq!(
            estimate_request_context_tokens(&bare),
            expected("bare_messages_list")
        );
        assert_eq!(
            estimate_request_context_tokens(&chat),
            expected("chat_completions_dict_with_tools")
        );
        assert_eq!(
            estimate_request_context_tokens(&responses),
            expected("responses_api_dict_input_instructions")
        );
    }

    #[test]
    fn config_precedence_and_safe_clamp_are_frozen() {
        let config = json!({
            "agent": {
                "local_stream_stale_timeout": 321
            },
            "providers": {"openrouter": {
                "request_timeout_seconds": 77,
                "stale_timeout_seconds": 120,
                "models": {"model": {
                    "timeout_seconds": 42,
                    "stale_timeout_seconds": 61
                }}
            }}
        });
        let policy = Policy::resolve(&config, "openrouter", "model", |name| match name {
            "HERMES_API_TIMEOUT" => Some("999".into()),
            "HERMES_STREAM_STALE_TIMEOUT" => Some("998".into()),
            "HERMES_STREAM_RETRIES" => Some("4".into()),
            "HERMES_STREAM_STALE_GIVEUP" => Some("7".into()),
            _ => None,
        });
        assert_eq!(policy.request_timeout(), Duration::from_secs(42));
        assert_eq!(policy.stream_attempts(), 5);
        assert_eq!(policy.stale_giveup(), 7);
        assert_eq!(
            policy.stream_stale_timeout("https://api.example.test", "model", &json!({})),
            Duration::from_secs(61)
        );
        assert_eq!(
            policy.stream_inactivity_timeout("https://api.example.test", "model", &json!({})),
            Duration::from_secs(42)
        );

        assert_eq!(
            positive_seconds(&json!(40_000_000)),
            Some(MAX_SAFE_TIMEOUT_SECONDS)
        );
        assert_eq!(
            positive_seconds(&json!("inf")),
            Some(MAX_SAFE_TIMEOUT_SECONDS)
        );
        assert_eq!(positive_seconds(&json!(true)), Some(1.0));
        assert_eq!(positive_seconds(&json!(false)), None);

        let local = Policy::resolve(&config, "missing", "plain-model", |name| {
            (name == "HERMES_LOCAL_STREAM_STALE_TIMEOUT").then(|| "654".into())
        });
        assert_eq!(
            local.stream_stale_timeout("http://localhost:11434", "plain-model", &json!({})),
            Duration::from_secs(654)
        );
    }

    #[test]
    fn local_and_context_scaling_keep_distinct_streaming_contracts() {
        let default = Policy::default();
        let huge = json!({"messages":[{"role":"user", "content":"x".repeat(480_000)}]});

        assert_eq!(
            default.stream_stale_timeout("http://127.0.0.1:11434", "o1", &huge),
            Duration::from_secs(900)
        );
        assert_eq!(
            default.buffered_stale_timeout("http://ollama:11434", &huge),
            None
        );
        assert_eq!(
            default.stream_stale_timeout("https://api.example.test", "gpt-4o", &huge),
            Duration::from_secs(300)
        );
        assert_eq!(
            default.buffered_stale_timeout("https://api.example.test", &huge),
            Some(Duration::from_secs(240))
        );

        let short_read = Policy::resolve(&json!({}), "fixture", "model", |name| {
            (name == "HERMES_STREAM_READ_TIMEOUT").then(|| "10".into())
        });
        assert_eq!(
            short_read.stream_inactivity_timeout("https://api.example.test", "model", &json!({})),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn run_budget_caps_an_implicit_reasoning_floor() {
        let corpus = run_budget_goldens();
        let golden = corpus["buffered_stale"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["case_name"] == "elapsed_reasoning_floor_uses_remaining_budget")
            .unwrap();
        let policy = Policy::resolve(&json!({}), "deepseek", "deepseek/deepseek-r1", |_| None);

        assert_eq!(
            policy.buffered_stale_timeout_capped(
                "https://api.deepseek.com",
                &json!({"messages":[{"role":"user", "content":"hi"}]}),
                Some(
                    golden["run_budget_seconds"].as_f64().unwrap()
                        - golden["elapsed_seconds"].as_f64().unwrap(),
                ),
            ),
            Some(Duration::from_secs_f64(
                golden["expected_timeout"].as_f64().unwrap()
            ))
        );
    }

    #[test]
    fn run_budget_config_normalization_matches_python() {
        for case in run_budget_goldens()["normalization"].as_array().unwrap() {
            let raw = &case["raw_input"];
            let expected = match &case["expected_seconds"] {
                Value::Null => None,
                Value::String(value) if value == "inf" => Some(f64::INFINITY),
                Value::Number(value) => value.as_f64(),
                value => panic!("unexpected normalization fixture: {value}"),
            };
            assert_eq!(
                normalize_run_budget_seconds(raw),
                expected,
                "{}",
                case["case_name"]
            );
        }
    }

    #[test]
    fn run_budget_buffered_policy_matches_python_goldens() {
        for case in run_budget_goldens()["buffered_stale"].as_array().unwrap() {
            let provider = case["provider"].as_str().unwrap_or("openai");
            let model = case["model"].as_str().unwrap_or("gpt-4o");
            let explicit = case["explicit"].as_str();
            let explicit_seconds = case["explicit_seconds"].as_f64();
            let config = match explicit {
                Some("model") => json!({"providers": {provider: {"models": {
                    model: {"stale_timeout_seconds": explicit_seconds.unwrap()}
                }}}}),
                Some("provider") => json!({"providers": {provider: {
                    "stale_timeout_seconds": explicit_seconds.unwrap()
                }}}),
                _ => json!({}),
            };
            let policy = Policy::resolve(&config, provider, model, |name| {
                (explicit == Some("environment") && name == "HERMES_API_CALL_STALE_TIMEOUT")
                    .then(|| explicit_seconds.unwrap().to_string())
            });
            let body = match case["payload_kind"].as_str().unwrap_or("small") {
                "small" => json!({"messages":[{"role":"user", "content":"hi"}]}),
                "medium" => json!({"input":"m".repeat(240_004)}),
                "large" => json!({"input":"L".repeat(440_004)}),
                kind => panic!("unexpected payload kind: {kind}"),
            };
            let remaining = case["elapsed_seconds"]
                .as_f64()
                .map(|elapsed| case["run_budget_seconds"].as_f64().unwrap() - elapsed);
            let actual = policy.buffered_stale_timeout_capped(
                case["base_url"]
                    .as_str()
                    .unwrap_or("https://api.example.test/v1"),
                &body,
                remaining,
            );
            let expected = if case["expected_timeout"] == "inf" {
                None
            } else {
                Some(Duration::from_secs_f64(
                    case["expected_timeout"].as_f64().unwrap(),
                ))
            };
            assert_eq!(actual, expected, "{}", case["case_name"]);
        }
    }

    #[test]
    fn run_budget_does_not_change_streaming_goldens() {
        for case in run_budget_goldens()["streaming_unchanged"]
            .as_array()
            .unwrap()
        {
            let body = match case["payload_kind"].as_str().unwrap() {
                "small" => json!({"messages":[{"role":"user", "content":"hi"}]}),
                "large" => json!({"input":"L".repeat(440_004)}),
                kind => panic!("unexpected payload kind: {kind}"),
            };
            let policy = Policy::resolve(
                &json!({}),
                case["provider"].as_str().unwrap_or("openai"),
                case["model"].as_str().unwrap(),
                |_| None,
            );
            assert_eq!(
                policy.stream_stale_timeout(
                    "https://api.example.test/v1",
                    case["model"].as_str().unwrap(),
                    &body,
                ),
                Duration::from_secs_f64(case["expected_timeout"].as_f64().unwrap()),
                "{}",
                case["case_name"]
            );
        }
    }

    #[test]
    fn run_budget_cap_preserves_python_ordering_and_bounds() {
        let body = json!({"messages":[{"role":"user", "content":"hi"}]});
        let default = Policy::default();
        assert_eq!(
            default.buffered_stale_timeout_capped("https://api.example.test", &body, Some(-10.0),),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            default.buffered_stale_timeout_capped(
                "https://api.example.test",
                &body,
                Some(10_000.0),
            ),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            default.buffered_stale_timeout_capped("http://localhost:11434", &body, Some(1.0)),
            None
        );

        let explicit = Policy::resolve(
            &json!({"providers":{"fixture":{"stale_timeout_seconds":600}}}),
            "fixture",
            "deepseek/deepseek-r1",
            |_| None,
        );
        assert_eq!(
            explicit.buffered_stale_timeout_capped("https://api.example.test", &body, Some(1.0),),
            Some(Duration::from_secs(600))
        );
        let explicit_env = Policy::resolve(&json!({}), "fixture", "plain", |name| {
            (name == "HERMES_API_CALL_STALE_TIMEOUT").then(|| "1200".into())
        });
        assert_eq!(
            explicit_env.buffered_stale_timeout_capped(
                "https://api.example.test",
                &body,
                Some(1.0),
            ),
            Some(Duration::from_secs(1200))
        );

        let huge = json!({"messages":[{"role":"user", "content":"x".repeat(480_000)}]});
        assert_eq!(
            default.buffered_stale_timeout_capped("https://api.example.test", &huge, Some(200.0),),
            Some(Duration::from_secs(100))
        );
        assert_eq!(
            default.stream_stale_timeout("https://api.example.test", "gpt-4o", &huge),
            Duration::from_secs(300)
        );
    }
}
