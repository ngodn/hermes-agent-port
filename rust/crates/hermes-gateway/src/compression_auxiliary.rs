//! Frozen configuration for the full-compression auxiliary request.
//!
//! Route construction stays in startup, where profile-scoped credentials are
//! already available. This module owns only coercion and the fast-lane gate so
//! summary request policy cannot leak into normal conversation calls.

use serde_json::{Map, Value};
use std::collections::HashMap;

const DEFAULT_TIMEOUT_SECONDS: f64 = 120.0;
const COMPRESSION_TIMEOUT_FLOOR_SECONDS: f64 = 300.0;

#[derive(Clone)]
pub(crate) struct Config {
    pub provider: String,
    fast_lane_provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub key_env: Option<String>,
    pub api_mode: Option<String>,
    pub timeout: std::time::Duration,
    pub extra_body: Map<String, Value>,
    pub reasoning_config: Option<Value>,
    pub max_output_tokens: Option<u64>,
    pub needs_separate_client: bool,
}

impl Config {
    pub fn from_value(root: &Value) -> Self {
        let section = root
            .get("auxiliary")
            .and_then(|value| value.get("compression"))
            .and_then(Value::as_object);
        let empty = Map::new();
        let section = section.unwrap_or(&empty);

        let mut provider = text(section.get("provider"))
            .unwrap_or_else(|| "auto".into())
            .to_lowercase();
        let fast_lane_provider = provider.clone();
        let model = text(section.get("model")).filter(|value| !value.eq_ignore_ascii_case("auto"));
        let mut base_url = text(section.get("base_url"));
        let api_key = text(section.get("api_key"));
        let key_env = section
            .get("key_env")
            .filter(|value| crate::python_value::truthy(value))
            .or_else(|| {
                section
                    .get("api_key_env")
                    .filter(|value| crate::python_value::truthy(value))
            })
            .and_then(|value| text(Some(value)));
        let api_mode = text(section.get("api_mode")).map(normalize_api_mode);
        let extra_body = section
            .get("extra_body")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let reasoning_config = section
            .get("reasoning_effort")
            .filter(|value| !value.is_null() && value.as_str() != Some(""))
            .and_then(crate::reasoning_effort::parse_value);
        let max_output_tokens = positive_integer(section.get("max_output_tokens"));
        let timeout = timeout_seconds(section.get("timeout"));
        if provider == "openai" {
            provider = "custom".into();
            base_url.get_or_insert_with(|| "https://api.openai.com/v1".into());
        } else if provider == "auto" && base_url.is_some() && api_key.is_some() {
            provider = "custom".into();
        } else if provider == "auto" && base_url.is_some() {
            // A bare endpoint is not enough to select a custom provider. The
            // Python resolver drops it and continues through main/auto routing.
            base_url = None;
        }
        let needs_separate_client = provider != "auto"
            || model.is_some()
            || base_url.is_some()
            || api_key.is_some()
            || key_env.is_some()
            || api_mode.is_some()
            || !extra_body.is_empty()
            || reasoning_config.is_some()
            || max_output_tokens.is_some();

        Self {
            provider,
            fast_lane_provider,
            model,
            base_url,
            api_key,
            key_env,
            api_mode,
            timeout,
            extra_body,
            reasoning_config,
            max_output_tokens,
            needs_separate_client,
        }
    }

    /// A hard summary cap is safe only for an exact concrete route explicitly
    /// certified as non-reasoning. Auto or drifted fallbacks remain uncapped.
    pub fn certifies_route(&self, actual_provider: &str, actual_model: &str) -> bool {
        let requested_provider = normalize_provider(&self.fast_lane_provider);
        let actual_provider = normalize_provider(actual_provider);
        let explicit_route =
            !matches!(requested_provider.as_str(), "" | "auto") && self.model.is_some();
        let non_reasoning = self
            .reasoning_config
            .as_ref()
            .is_some_and(|value| value.get("enabled").and_then(Value::as_bool) == Some(false));
        explicit_route
            && requested_provider == actual_provider
            && self
                .model
                .as_deref()
                .is_some_and(|model| model.eq_ignore_ascii_case(actual_model))
            && non_reasoning
    }

    pub fn certified_output_cap(&self, actual_provider: &str, actual_model: &str) -> Option<u64> {
        self.certifies_route(actual_provider, actual_model)
            .then_some(self.max_output_tokens)
            .flatten()
    }

    pub fn direct_api_key(
        &self,
        dotenv: &HashMap<String, String>,
        mut environment: impl FnMut(&str) -> Option<String>,
    ) -> Option<String> {
        self.api_key.clone().or_else(|| {
            self.key_env.as_ref().and_then(|name| {
                environment(name)
                    .or_else(|| dotenv.get(name).cloned())
                    .map(|value| {
                        value
                            .trim_matches(crate::python_value::python_whitespace)
                            .to_owned()
                    })
                    .filter(|value| !value.is_empty())
            })
        })
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(value))
                .trim_matches(crate::python_value::python_whitespace)
                .to_owned()
        })
        .filter(|value| !value.is_empty())
}

fn positive_integer(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if value.is_boolean() {
        return None;
    }
    crate::python_value::integer(value)
        .and_then(|value| value.as_u64())
        .filter(|value| *value > 0)
}

fn timeout_seconds(value: Option<&Value>) -> std::time::Duration {
    let parsed = match value {
        Some(Value::Number(value)) => value.as_f64(),
        Some(Value::String(value)) => value
            .trim_matches(crate::python_value::python_whitespace)
            .parse::<f64>()
            .ok(),
        _ => None,
    }
    .filter(|value| value.is_finite() && *value > 0.0)
    .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
    .max(COMPRESSION_TIMEOUT_FLOOR_SECONDS);
    std::time::Duration::from_secs_f64(parsed)
}

pub(crate) fn normalize_api_mode(value: String) -> String {
    match value.to_lowercase().as_str() {
        "openai" | "openai_chat" | "openai-chat" | "chat-completions" | "chatcompletions" => {
            "chat_completions".into()
        }
        "responses" | "openai_responses" | "openai-responses" => "codex_responses".into(),
        "anthropic" | "anthropic-messages" | "messages" => "anthropic_messages".into(),
        "bedrock" | "bedrock-converse" => "bedrock_converse".into(),
        _ => value,
    }
}

fn normalize_provider(value: &str) -> String {
    match value
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase()
        .as_str()
    {
        "google" | "google-gemini" | "google-ai-studio" => "gemini".into(),
        "x-ai" | "x.ai" | "grok" => "xai".into(),
        "glm" | "z-ai" | "z.ai" | "zhipu" => "zai".into(),
        "kimi" | "moonshot" => "kimi-coding".into(),
        "kimi-cn" | "moonshot-cn" => "kimi-coding-cn".into(),
        "gmi-cloud" | "gmicloud" => "gmi".into(),
        "actual-computer" | "actualcomputer" | "aci" => "actual".into(),
        "minimax-china" | "minimax_cn" => "minimax-cn".into(),
        "claude" | "claude-code" => "anthropic".into(),
        "github" | "github-copilot" | "github-model" | "github-models" => "copilot".into(),
        "github-copilot-acp" | "copilot-acp-agent" => "copilot-acp".into(),
        "tencent" | "tokenhub" | "tencent-cloud" | "tencentmaas" => "tencent-tokenhub".into(),
        "tokenplan" | "tencent-lkeap" => "tencent-tokenplan".into(),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use serde_json::json;

    #[test]
    fn defaults_inherit_main_route_and_apply_compression_timeout_floor() {
        let config = Config::from_value(&json!({}));
        assert_eq!(config.provider, "auto");
        assert!(config.model.is_none());
        assert!(!config.needs_separate_client);
        assert_eq!(config.timeout.as_secs(), 300);
        assert_eq!(config.certified_output_cap("custom", "main"), None);
    }

    #[test]
    fn hard_cap_requires_exact_concrete_non_reasoning_route() {
        let config = Config::from_value(&json!({"auxiliary":{"compression":{
            "provider":"OpenAI",
            "model":"gpt-4o-mini",
            "reasoning_effort":false,
            "max_output_tokens":"2048",
            "timeout":700
        }}}));
        assert!(config.needs_separate_client);
        assert_eq!(config.timeout.as_secs(), 700);
        assert_eq!(config.certified_output_cap("custom", "gpt-4o-mini"), None);
        assert_eq!(config.certified_output_cap("custom", "gpt-4o"), None);

        let custom = Config::from_value(&json!({"auxiliary":{"compression":{
            "provider":"custom", "model":"gpt-4o-mini",
            "reasoning_effort":false, "max_output_tokens":2048
        }}}));
        assert_eq!(
            custom.certified_output_cap("custom", "gpt-4o-mini"),
            Some(2_048)
        );

        let reasoning = Config::from_value(&json!({"auxiliary":{"compression":{
            "provider":"custom", "model":"m", "reasoning_effort":"high",
            "max_output_tokens":10
        }}}));
        assert_eq!(reasoning.certified_output_cap("custom", "m"), None);
    }

    #[test]
    fn route_selection_matches_python_config_boundaries() {
        let bare = Config::from_value(
            &json!({"auxiliary":{"compression":{"base_url":"https://relay/v1"}}}),
        );
        assert_eq!(bare.provider, "auto");
        assert!(bare.base_url.is_none());
        assert!(!bare.needs_separate_client);

        let keyed = Config::from_value(&json!({"auxiliary":{"compression":{
            "base_url":"https://relay/v1", "api_key":"key"
        }}}));
        assert_eq!(keyed.provider, "custom");
        assert_eq!(keyed.base_url.as_deref(), Some("https://relay/v1"));

        let openai = Config::from_value(&json!({"auxiliary":{"compression":{
            "provider":"openai", "model":"gpt-4o"
        }}}));
        assert_eq!(openai.provider, "custom");
        assert_eq!(
            openai.base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
    }

    #[test]
    fn task_config_resolution_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-auxiliary-config-goldens.json"
        ))
        .unwrap();
        for case in corpus["task_config_resolution"].as_array().unwrap() {
            if !case["explicit_args"].is_null() {
                continue;
            }
            let root = case["config"].as_object().map_or_else(
                || json!({}),
                |config| json!({"auxiliary":{"compression":config}}),
            );
            let parsed = Config::from_value(&root);
            let environment = case["env"].as_object();
            let key = parsed.direct_api_key(&Default::default(), |name| {
                environment
                    .and_then(|values| values.get(name))
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            });
            let expected = &case["resolved"];
            assert_eq!(
                parsed.provider,
                expected["provider"].as_str().unwrap(),
                "{case}"
            );
            assert_eq!(
                parsed.model.as_deref(),
                expected["model"].as_str(),
                "{case}"
            );
            assert_eq!(
                parsed.base_url.as_deref(),
                expected["base_url"].as_str(),
                "{case}"
            );
            assert_eq!(key.as_deref(), expected["api_key"].as_str(), "{case}");
            assert_eq!(
                parsed.api_mode.as_deref(),
                expected["api_mode"].as_str(),
                "{case}"
            );
        }
    }

    #[test]
    fn fast_lane_certification_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-auxiliary-config-goldens.json"
        ))
        .unwrap();
        for case in corpus["summary_output_cap"]["fast_lane"]
            .as_array()
            .unwrap()
        {
            if !case["requested_provider"].is_null() || !case["requested_model"].is_null() {
                continue;
            }
            let parsed = Config::from_value(
                &json!({"auxiliary":{"compression":case["route_config"].clone()}}),
            );
            let provider = case["actual_provider"].as_str().unwrap();
            let model = case["actual_model"].as_str().unwrap_or("");
            assert_eq!(
                parsed.certifies_route(provider, model),
                case["result"]["certified_non_reasoning"].as_bool().unwrap(),
                "{case}"
            );
            assert_eq!(
                parsed.certified_output_cap(provider, model),
                case["result"]["max_tokens"].as_u64(),
                "{case}"
            );
        }
    }
}
