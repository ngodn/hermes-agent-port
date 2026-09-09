//! Frozen configuration for the full-compression auxiliary request.
//!
//! Route construction stays in startup, where profile-scoped credentials are
//! already available. This module owns only coercion and the fast-lane gate so
//! summary request policy cannot leak into normal conversation calls.

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

const DEFAULT_TIMEOUT_SECONDS: f64 = 120.0;
const COMPRESSION_TIMEOUT_FLOOR_SECONDS: f64 = 300.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureScope {
    Model,
    Credential,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackendIdentity {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    first_class_provider: bool,
}

impl BackendIdentity {
    pub fn new(provider: &str, model: &str, base_url: &str) -> Self {
        Self::new_with_provider_kind(provider, model, base_url, false)
    }

    pub fn new_with_provider_kind(
        provider: &str,
        model: &str,
        base_url: &str,
        first_class_provider: bool,
    ) -> Self {
        Self {
            provider: provider
                .trim_matches(crate::python_value::python_whitespace)
                .to_lowercase(),
            model: model
                .trim_matches(crate::python_value::python_whitespace)
                .to_lowercase(),
            base_url: base_url
                .trim_matches(crate::python_value::python_whitespace)
                .trim_end_matches('/')
                .to_lowercase(),
            first_class_provider,
        }
    }
}

pub(crate) fn classify_failure_reason(reason: &str) -> FailureScope {
    match reason
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase()
        .as_str()
    {
        "auth error" | "payment error" => FailureScope::Credential,
        _ => FailureScope::Model,
    }
}

pub(crate) fn should_skip_candidate(
    candidate: &BackendIdentity,
    failed: &BackendIdentity,
    scope: FailureScope,
) -> bool {
    match scope {
        FailureScope::Credential => {
            if !candidate.provider.is_empty() && !failed.provider.is_empty() {
                candidate.provider == failed.provider
            } else {
                !candidate.base_url.is_empty() && candidate.base_url == failed.base_url
            }
        }
        FailureScope::Model => {
            if candidate.model.is_empty() || candidate.model != failed.model {
                return false;
            }
            if candidate.provider != failed.provider || candidate.provider.is_empty() {
                return !candidate.first_class_provider
                    && !failed.first_class_provider
                    && !candidate.base_url.is_empty()
                    && candidate.base_url == failed.base_url;
            }
            candidate.base_url.is_empty()
                || failed.base_url.is_empty()
                || candidate.base_url == failed.base_url
        }
    }
}

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
    /// Ordered `auxiliary.compression.fallback_chain` entries, parsed in
    /// source order. The primary lane resolves these into native clients at
    /// startup; this lane only owns the typed shape and coercion.
    pub fallback_chain: Vec<FallbackChainEntry>,
}

/// One ordered `auxiliary.compression.fallback_chain` entry.
///
/// Mirrors the fields the Python summary fallback path reads off each entry
/// dict (`agent/auxiliary_client.py`): `provider` is required, the rest are
/// optional. Resolution into a client, provider-profile lookup, credential
/// pool rotation, and OAuth refresh all stay in startup; this struct carries
/// only the parsed configuration plus the direct/key-env credential lookup.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FallbackChainEntry {
    pub provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub key_env: Option<String>,
    pub api_mode: Option<String>,
    /// Per-entry timeout. Independent of the task-level compression timeout
    /// floor: the ordinary Python fallback path (`_coerce_positive_timeout`
    /// plus `_fallback_entry_timeout`) applies no 300s floor here.
    pub timeout: Option<std::time::Duration>,
    pub reasoning_config: Option<Value>,
    pub max_output_tokens: Option<u64>,
    /// Main-turn provider fallback carries this provider-local replay opt-in.
    /// Auxiliary compression ignores it because summaries have no replay.
    pub reasoning_echo: bool,
}

impl FallbackChainEntry {
    /// Parse one chain element. Returns `None` for non-object entries and for
    /// entries without a nonempty `provider`, matching the Python loop in
    /// `_try_configured_fallback_chain` which skips both.
    pub fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        // Python keeps the entry's original case in the dict but every routing
        // consumer (`resolve_compression_fast_lane`, `BackendIdentity`) lowercases
        // before comparing, so lowercasing here is behavior preserving and matches
        // the primary `Config` provider convention above.
        let provider = text(object.get("provider"))?.to_lowercase();
        if provider.is_empty() {
            return None;
        }
        // Unlike the primary section, a fallback entry keeps a literal "auto"
        // model; Python passes it straight to the router rather than dropping it.
        let model = text(object.get("model"));
        let base_url = text(object.get("base_url"));
        let api_key = text(object.get("api_key"));
        let key_env = object
            .get("key_env")
            .filter(|value| crate::python_value::truthy(value))
            .or_else(|| {
                object
                    .get("api_key_env")
                    .filter(|value| crate::python_value::truthy(value))
            })
            .and_then(|value| text(Some(value)));
        // `api_mode` wins over the `transport` alias, matching the Python
        // `entry.get("api_mode") or entry.get("transport")` precedence.
        let api_mode = text(object.get("api_mode"))
            .or_else(|| text(object.get("transport")))
            .map(normalize_api_mode);
        let timeout = entry_timeout_seconds(object.get("timeout"));
        let reasoning_config = object
            .get("reasoning_effort")
            .filter(|value| !value.is_null() && value.as_str() != Some(""))
            .and_then(crate::reasoning_effort::parse_value);
        let max_output_tokens = positive_integer(object.get("max_output_tokens"));
        let reasoning_echo = object
            .get("reasoning_echo")
            .is_some_and(crate::python_value::truthy);

        Some(Self {
            provider,
            model,
            base_url,
            api_key,
            key_env,
            api_mode,
            timeout,
            reasoning_config,
            max_output_tokens,
            reasoning_echo,
        })
    }

    /// Candidate-local fast-lane controls are valid only when the configured
    /// provider and model resolve to this exact non-reasoning route.
    pub fn certifies_route(&self, actual_provider: &str, actual_model: &str) -> bool {
        let requested_provider = normalize_provider(&self.provider);
        let actual_provider = normalize_provider(actual_provider);
        let non_reasoning = self
            .reasoning_config
            .as_ref()
            .is_some_and(|value| value.get("enabled").and_then(Value::as_bool) == Some(false));
        requested_provider == actual_provider
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

    /// Resolve the entry's API key from an inline `api_key` or a `key_env` /
    /// `api_key_env` name, mirroring `Config::direct_api_key`. Provider-profile
    /// and pool credential resolution stay in startup by design.
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

/// Merge the modern and legacy main-agent fallback settings exactly as
/// `hermes_cli.fallback_config.get_fallback_chain` does. Unlike the task chain,
/// every retained entry must name both a provider and model.
pub(crate) fn main_fallback_chain(root: &Value) -> Vec<FallbackChainEntry> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for key in ["fallback_providers", "fallback_model"] {
        let Some(raw) = root.get(key) else {
            continue;
        };
        let values: Vec<&Value> = match raw {
            Value::Object(_) => vec![raw],
            Value::Array(values) => values.iter().collect(),
            _ => Vec::new(),
        };
        for value in values {
            let Some(object) = value.as_object() else {
                continue;
            };
            let Some(provider) = object
                .get("provider")
                .filter(|value| crate::python_value::truthy(value))
                .and_then(|value| text(Some(value)))
            else {
                continue;
            };
            let Some(model) = object
                .get("model")
                .filter(|value| crate::python_value::truthy(value))
                .and_then(|value| text(Some(value)))
            else {
                continue;
            };
            let base_url = object
                .get("base_url")
                .and_then(Value::as_str)
                .map(|value| {
                    value
                        .trim_matches(crate::python_value::python_whitespace)
                        .trim_end_matches('/')
                        .to_owned()
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_default();
            let identity = (
                provider.to_lowercase(),
                model.to_lowercase(),
                base_url.to_lowercase(),
            );
            if !seen.insert(identity) {
                continue;
            }
            let mut normalized = object.clone();
            normalized.insert("provider".into(), Value::String(provider));
            normalized.insert("model".into(), Value::String(model));
            if !base_url.is_empty() {
                normalized.insert("base_url".into(), Value::String(base_url));
            } else if normalized
                .get("base_url")
                .is_some_and(|value| !crate::python_value::truthy(value))
            {
                normalized.remove("base_url");
            }
            if let Some(entry) = FallbackChainEntry::from_value(&Value::Object(normalized)) {
                result.push(entry);
            }
        }
    }
    result
}

pub(crate) fn should_skip_main_fallback_provider(
    candidate_provider: &str,
    failed_provider: &str,
    main_provider: &str,
) -> bool {
    let candidate = normalize_provider(candidate_provider);
    let failed = normalize_provider(failed_provider);
    let main = normalize_provider(main_provider);
    candidate == "auto"
        || (!failed.is_empty() && candidate == failed)
        || (!main.is_empty() && candidate == main)
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
        let fallback_chain = section
            .get("fallback_chain")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(FallbackChainEntry::from_value)
                    .collect()
            })
            .unwrap_or_default();
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
            fallback_chain,
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

    pub fn claims_fast_lane(&self) -> bool {
        let provider = normalize_provider(&self.fast_lane_provider);
        !matches!(provider.as_str(), "" | "auto")
            && self.model.is_some()
            && self
                .reasoning_config
                .as_ref()
                .is_some_and(|value| value.get("enabled").and_then(Value::as_bool) == Some(false))
            && self.max_output_tokens.is_some()
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

/// Per-entry timeout coercion for a fallback-chain entry.
///
/// Faithful to Python `_coerce_positive_timeout`: accept only a positive,
/// finite numeric value; reject booleans, non-positive numbers, strings,
/// null, and containers. Unlike the task-level `timeout_seconds`, no default
/// is substituted and the 300s compression floor is never applied, so an
/// entry either carries its own independent budget or `None` (keep the
/// task-level timeout).
fn entry_timeout_seconds(value: Option<&Value>) -> Option<std::time::Duration> {
    let value = value?;
    if value.is_boolean() {
        return None;
    }
    let parsed = match value {
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
    .filter(|value| value.is_finite() && *value > 0.0)?;
    Some(std::time::Duration::from_secs_f64(parsed))
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
    use super::{
        classify_failure_reason, entry_timeout_seconds, main_fallback_chain, normalize_api_mode,
        should_skip_candidate, should_skip_main_fallback_provider, BackendIdentity, Config,
        FailureScope, FallbackChainEntry,
    };
    use serde_json::json;

    #[test]
    fn fallback_chain_defaults_empty_and_ignores_non_list() {
        assert!(Config::from_value(&json!({})).fallback_chain.is_empty());
        let non_list = Config::from_value(&json!({"auxiliary":{"compression":{
            "fallback_chain":"none"
        }}}));
        assert!(non_list.fallback_chain.is_empty());
    }

    #[test]
    fn main_fallback_chain_merges_modern_then_legacy_and_deduplicates_routes() {
        let chain = main_fallback_chain(&json!({
            "fallback_providers":[
                "invalid",
                {"provider":" OpenRouter ", "model":" m ", "base_url":" https://a/v1/ "},
                {"provider":"anthropic", "model":"claude"},
                {"provider":"missing-model"}
            ],
            "fallback_model":[
                {"provider":"openrouter", "model":"M", "base_url":"https://a/v1"},
                {"provider":"custom", "model":"local"}
            ]
        }));
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].provider, "openrouter");
        assert_eq!(chain[0].model.as_deref(), Some("m"));
        assert_eq!(chain[0].base_url.as_deref(), Some("https://a/v1"));
        assert_eq!(chain[1].provider, "anthropic");
        assert_eq!(chain[2].provider, "custom");

        let singleton = main_fallback_chain(&json!({
            "fallback_model":{"provider":"custom", "model":"one"}
        }));
        assert_eq!(singleton.len(), 1);
        assert_eq!(singleton[0].model.as_deref(), Some("one"));
    }

    #[test]
    fn main_fallback_chain_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-main-fallback-chain-goldens.json"
        ))
        .unwrap();
        for section in [
            "container_and_entry_parsing",
            "chain_deduplication_and_identity",
        ] {
            for case in corpus[section].as_array().unwrap() {
                let actual = main_fallback_chain(&case["raw_config"]);
                let expected = case["chain"].as_array().unwrap();
                assert_eq!(actual.len(), expected.len(), "{case}");
                for (actual, expected) in actual.iter().zip(expected) {
                    assert_eq!(
                        actual.provider,
                        expected["provider"].as_str().unwrap().to_lowercase(),
                        "{case}"
                    );
                    assert_eq!(
                        actual.model.as_deref(),
                        expected["model"].as_str(),
                        "{case}"
                    );
                    let expected_base = expected
                        .get("base_url")
                        .filter(|value| crate::python_value::truthy(value))
                        .and_then(|value| super::text(Some(value)));
                    assert_eq!(actual.base_url, expected_base, "{case}");
                }
            }
        }
    }

    #[test]
    fn main_turn_fallback_parser_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/main-provider-fallback-goldens.json"
        ))
        .unwrap();
        for case in corpus["container_and_chain_parsing"].as_array().unwrap() {
            let chain = main_fallback_chain(&case["input_config"]);
            let identities: Vec<_> = chain
                .iter()
                .map(|entry| {
                    json!([
                        entry.provider.to_lowercase(),
                        entry.model.as_deref().unwrap_or("").to_lowercase(),
                        entry
                            .base_url
                            .as_deref()
                            .unwrap_or("")
                            .trim_end_matches('/')
                            .to_lowercase(),
                    ])
                })
                .collect();
            assert_eq!(
                chain.len(),
                case["entry_count"].as_u64().unwrap() as usize,
                "{case}"
            );
            assert_eq!(
                identities,
                *case["resolved_identities"].as_array().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn main_turn_backend_skip_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/main-provider-fallback-goldens.json"
        ))
        .unwrap();
        for case in corpus["chain_traversal_bounds_and_skip_dedup"]
            .as_array()
            .unwrap()
        {
            let Some(candidate) = case.get("candidate") else {
                continue;
            };
            let failed = &case["failed_target"];
            let first_class = |provider: &str| matches!(provider, "xai" | "xai-oauth");
            let candidate_provider = candidate["provider"].as_str().unwrap_or("");
            let failed_provider = failed["provider"].as_str().unwrap_or("");
            let candidate = BackendIdentity::new_with_provider_kind(
                candidate_provider,
                candidate["model"].as_str().unwrap_or(""),
                candidate["base_url"].as_str().unwrap_or(""),
                first_class(candidate_provider),
            );
            let failed = BackendIdentity::new_with_provider_kind(
                failed_provider,
                failed["model"].as_str().unwrap_or(""),
                failed["base_url"].as_str().unwrap_or(""),
                first_class(failed_provider),
            );
            assert_eq!(
                should_skip_candidate(&candidate, &failed, FailureScope::Model),
                case["should_skip"].as_bool().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn main_fallback_provider_skip_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-main-fallback-chain-goldens.json"
        ))
        .unwrap();
        for case in corpus["provider_skipping_and_health_rules"]
            .as_array()
            .unwrap()
        {
            if case["unhealthy_active"].as_bool() == Some(true) {
                continue;
            }
            let selected = case["chain"].as_array().unwrap().iter().find(|entry| {
                !should_skip_main_fallback_provider(
                    entry["provider"].as_str().unwrap(),
                    case["failed_provider"].as_str().unwrap(),
                    case["main_provider"].as_str().unwrap(),
                )
            });
            assert_eq!(
                selected.map(|entry| entry["provider"].as_str().unwrap()),
                case["resolved_provider"]
                    .as_str()
                    .filter(|provider| !provider.is_empty()),
                "{case}"
            );
        }
    }

    #[test]
    fn main_fallback_context_floor_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-main-fallback-chain-goldens.json"
        ))
        .unwrap();
        for case in corpus["compression_context_window_filtering_64k"]
            .as_array()
            .unwrap()
        {
            let Some(chain) = case["chain"].as_array() else {
                continue;
            };
            let enforce_floor = case["task"] == "compression";
            let contexts = case["model_contexts"].as_object().unwrap();
            let selected = chain.iter().find(|entry| {
                !enforce_floor
                    || contexts[entry["model"].as_str().unwrap()]
                        .as_u64()
                        .is_none_or(|tokens| tokens >= 64_000)
            });
            assert_eq!(
                selected.map(|entry| entry["provider"].as_str().unwrap()),
                case["resolved_provider"].as_str(),
                "{case}"
            );
            assert_eq!(
                selected.map(|entry| entry["model"].as_str().unwrap()),
                case["resolved_model"].as_str(),
                "{case}"
            );
        }
    }

    #[test]
    fn main_fallback_credentials_and_transport_match_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-main-fallback-chain-goldens.json"
        ))
        .unwrap();
        for case in corpus["credential_and_transport_resolution"]
            .as_array()
            .unwrap()
        {
            let entry = FallbackChainEntry::from_value(&case["entry"]).unwrap();
            let key = entry.direct_api_key(&Default::default(), |name| match name {
                "FALLBACK_TEST_KEY" => Some("sk-synthetic-key-env-value".into()),
                "FALLBACK_TEST_ALIAS" => Some("sk-synthetic-api-key-env-alias".into()),
                _ => None,
            });
            assert_eq!(key.as_deref(), case["resolved_key_fc"].as_str(), "{case}");
            let expected_mode = case["api_mode_extracted"]
                .as_str()
                .map(str::to_owned)
                .map(normalize_api_mode);
            assert_eq!(entry.api_mode, expected_mode, "{case}");
        }
    }

    #[test]
    fn fallback_chain_keeps_source_order_and_rejects_invalid_entries() {
        let config = Config::from_value(&json!({"auxiliary":{"compression":{
            "fallback_chain":[
                "openrouter/llama-3",
                {"model":"gpt-4o"},
                {"provider":"   "},
                {"provider":"OpenRouter","model":"a"},
                42,
                {"provider":"nous","model":"b"}
            ]
        }}}));
        let providers: Vec<&str> = config
            .fallback_chain
            .iter()
            .map(|entry| entry.provider.as_str())
            .collect();
        assert_eq!(providers, ["openrouter", "nous"]);
        assert_eq!(config.fallback_chain[0].model.as_deref(), Some("a"));
        assert_eq!(config.fallback_chain[1].model.as_deref(), Some("b"));
    }

    #[test]
    fn fallback_entry_preserves_auto_model_and_normalizes_provider() {
        let entry = FallbackChainEntry::from_value(&json!({
            "provider":"  OpenRouter  ", "model":"auto"
        }))
        .unwrap();
        assert_eq!(entry.provider, "openrouter");
        assert_eq!(entry.model.as_deref(), Some("auto"));
        assert!(entry.base_url.is_none());
    }

    #[test]
    fn fallback_entry_timeout_is_independent_and_unfloored() {
        let integer = FallbackChainEntry::from_value(&json!({
            "provider":"p", "timeout":45
        }))
        .unwrap();
        assert_eq!(
            integer.timeout,
            Some(std::time::Duration::from_secs_f64(45.0))
        );

        let fractional = FallbackChainEntry::from_value(&json!({
            "provider":"p", "timeout":60.5
        }))
        .unwrap();
        assert_eq!(
            fractional.timeout,
            Some(std::time::Duration::from_secs_f64(60.5))
        );

        for rejected in [json!("45"), json!(true), json!(0), json!(-10), json!(null)] {
            let entry = FallbackChainEntry::from_value(&json!({
                "provider":"p", "timeout":rejected
            }))
            .unwrap();
            assert!(entry.timeout.is_none(), "timeout {rejected} should reject");
        }

        let omitted = FallbackChainEntry::from_value(&json!({"provider":"p"})).unwrap();
        assert!(omitted.timeout.is_none());
    }

    #[test]
    fn fallback_entry_api_mode_precedence_and_canonicalization() {
        let both = FallbackChainEntry::from_value(&json!({
            "provider":"p", "api_mode":"anthropic", "transport":"responses"
        }))
        .unwrap();
        assert_eq!(both.api_mode.as_deref(), Some("anthropic_messages"));

        let transport_only = FallbackChainEntry::from_value(&json!({
            "provider":"p", "transport":"chat-completions"
        }))
        .unwrap();
        assert_eq!(transport_only.api_mode.as_deref(), Some("chat_completions"));
    }

    #[test]
    fn fallback_entry_credential_precedence() {
        let inline = FallbackChainEntry::from_value(&json!({
            "provider":"p", "api_key":"sk-inline", "key_env":"VAR"
        }))
        .unwrap();
        assert_eq!(
            inline.direct_api_key(&Default::default(), |_| Some("env".into())),
            Some("sk-inline".into())
        );

        let key_env = FallbackChainEntry::from_value(&json!({
            "provider":"p", "key_env":"PRIMARY", "api_key_env":"ALIAS"
        }))
        .unwrap();
        assert_eq!(key_env.key_env.as_deref(), Some("PRIMARY"));
        assert_eq!(
            key_env.direct_api_key(&Default::default(), |name| (name == "PRIMARY")
                .then(|| "  from-env  ".into())),
            Some("from-env".into())
        );

        let alias = FallbackChainEntry::from_value(&json!({
            "provider":"p", "api_key_env":"ALIAS"
        }))
        .unwrap();
        assert_eq!(alias.key_env.as_deref(), Some("ALIAS"));
        assert!(alias
            .direct_api_key(&Default::default(), |_| None)
            .is_none());
    }

    #[test]
    fn fallback_entry_retains_reasoning_and_output_cap() {
        let entry = FallbackChainEntry::from_value(&json!({
            "provider":"custom", "model":"m",
            "reasoning_effort":false, "max_output_tokens":"2048"
        }))
        .unwrap();
        assert_eq!(entry.reasoning_config, Some(json!({"enabled": false})));
        assert_eq!(entry.max_output_tokens, Some(2_048));
        assert_eq!(entry.certified_output_cap("custom", "m"), Some(2_048));
        assert_eq!(entry.certified_output_cap("custom", "other"), None);

        let bool_cap = FallbackChainEntry::from_value(&json!({
            "provider":"p", "max_output_tokens":true
        }))
        .unwrap();
        assert!(bool_cap.max_output_tokens.is_none());
    }

    #[test]
    fn fallback_entries_match_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-fallback-chain-goldens.json"
        ))
        .unwrap();
        for case in corpus["entry_acceptance_and_coercion"].as_array().unwrap() {
            if case["case"] == "source_order_and_filtering_preserved" {
                let config = Config::from_value(&json!({"auxiliary":{"compression":{
                    "fallback_chain":case["raw"].clone()
                }}}));
                let expected = case["parsed"].as_array().unwrap();
                assert_eq!(config.fallback_chain.len(), expected.len(), "{case}");
                for (actual, expected) in config.fallback_chain.iter().zip(expected) {
                    assert_eq!(
                        actual.provider,
                        expected["provider"].as_str().unwrap(),
                        "{case}"
                    );
                    assert_eq!(
                        actual.model.as_deref(),
                        expected["model"].as_str(),
                        "{case}"
                    );
                }
                continue;
            }
            let actual = FallbackChainEntry::from_value(&case["raw"]);
            assert_eq!(
                actual.is_some(),
                case["accepted"].as_bool().unwrap(),
                "{case}"
            );
            let (Some(actual), Some(expected)) = (actual, case["parsed"].as_object()) else {
                continue;
            };
            assert_eq!(
                actual.provider,
                expected["provider"].as_str().unwrap(),
                "{case}"
            );
            assert_eq!(
                actual.model.as_deref(),
                expected["model"].as_str(),
                "{case}"
            );
            assert_eq!(
                actual.base_url.as_deref(),
                expected["base_url"].as_str(),
                "{case}"
            );
            assert_eq!(
                actual.api_key.as_deref(),
                expected["api_key"].as_str(),
                "{case}"
            );
            assert_eq!(
                actual.key_env.as_deref(),
                expected["key_env"].as_str(),
                "{case}"
            );
            assert_eq!(
                actual.api_mode.as_deref(),
                expected["api_mode"].as_str(),
                "{case}"
            );
            assert_eq!(
                actual.timeout.map(|timeout| timeout.as_secs_f64()),
                expected["timeout"].as_f64(),
                "{case}"
            );
            let expected_reasoning = expected
                .get("reasoning_effort")
                .filter(|value| !value.is_null());
            assert_eq!(
                actual.reasoning_config.as_ref(),
                expected_reasoning,
                "{case}"
            );
            assert_eq!(
                actual.max_output_tokens,
                expected["max_output_tokens"].as_u64(),
                "{case}"
            );
        }
    }

    #[test]
    fn fallback_scalar_rules_match_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-fallback-chain-goldens.json"
        ))
        .unwrap();
        for case in corpus["timeout_coercion"].as_array().unwrap() {
            let actual =
                entry_timeout_seconds(case.get("raw_value")).map(|timeout| timeout.as_secs_f64());
            assert_eq!(actual, case["coerced_timeout"].as_f64(), "{case}");
        }
        for case in corpus["api_mode_normalization"].as_array().unwrap() {
            let raw = case["raw_api_mode"].as_str().unwrap().trim().to_owned();
            assert_eq!(
                normalize_api_mode(raw),
                case["canonical_api_mode"].as_str().unwrap(),
                "{case}"
            );
        }
        for case in corpus["credential_resolution"].as_array().unwrap() {
            let Some(mut entry) = case["entry"].as_object().cloned() else {
                assert!(case["resolved_api_key"].is_null(), "{case}");
                continue;
            };
            entry.insert("provider".into(), json!("custom"));
            let entry = FallbackChainEntry::from_value(&serde_json::Value::Object(entry)).unwrap();
            let actual = entry.direct_api_key(&Default::default(), |name| match name {
                "SYNTHETIC_PRIMARY_KEY" => Some("sk-synthetic-env-key-1".into()),
                "SYNTHETIC_SECONDARY_KEY" => Some("sk-synthetic-env-key-2".into()),
                "EMPTY_KEY_VAR" => Some(String::new()),
                "WHITESPACE_KEY_VAR" => Some("   ".into()),
                _ => None,
            });
            assert_eq!(
                actual.as_deref(),
                case["resolved_api_key"].as_str(),
                "{case}"
            );
        }
    }

    #[test]
    fn fallback_route_skipping_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-fallback-chain-goldens.json"
        ))
        .unwrap();
        for case in corpus["failed_route_skipping"].as_array().unwrap() {
            if let Some(reason) = case["reason"].as_str() {
                let expected = match case["classified_scope"].as_str().unwrap() {
                    "credential" => FailureScope::Credential,
                    _ => FailureScope::Model,
                };
                assert_eq!(classify_failure_reason(reason), expected, "{case}");
                continue;
            }
            let candidate = BackendIdentity::new(
                case["candidate"]["provider"].as_str().unwrap_or(""),
                case["candidate"]["model"].as_str().unwrap_or(""),
                case["candidate"]["base_url"].as_str().unwrap_or(""),
            );
            let failed = BackendIdentity::new(
                case["failed_identity"]["provider"].as_str().unwrap_or(""),
                case["failed_identity"]["model"].as_str().unwrap_or(""),
                case["failed_identity"]["base_url"].as_str().unwrap_or(""),
            );
            let scope = match case["failure_scope"].as_str().unwrap() {
                "credential" => FailureScope::Credential,
                _ => FailureScope::Model,
            };
            assert_eq!(
                should_skip_candidate(&candidate, &failed, scope),
                case["should_skip"].as_bool().unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn fallback_traversal_matches_source_executed_python_corpus() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/compression-fallback-chain-goldens.json"
        ))
        .unwrap();
        for case in corpus["chain_traversal_and_exhaustion"].as_array().unwrap() {
            let name = case["case"].as_str().unwrap();
            if name == "stall_fallback_route_first_structurally_complete" {
                let selected = case["chain"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, raw)| {
                        FallbackChainEntry::from_value(raw)
                            .filter(|entry| entry.model.is_some())
                            .map(|entry| (index, entry))
                    })
                    .next()
                    .unwrap();
                assert_eq!(selected.0, 3, "{case}");
                assert_eq!(
                    selected.1.provider,
                    case["selected_route"]["provider"].as_str().unwrap(),
                    "{case}"
                );
                assert_eq!(
                    selected.1.model.as_deref(),
                    case["selected_route"]["model"].as_str(),
                    "{case}"
                );
                continue;
            }
            let failed_model = case["failed_model"].as_str().unwrap_or("");
            let failed = BackendIdentity::new(
                case["failed_provider"].as_str().unwrap_or(""),
                failed_model,
                "",
            );
            let scope = if failed_model.is_empty() {
                FailureScope::Credential
            } else {
                FailureScope::Model
            };
            if let Some(chain) = case["chain"].as_array() {
                let selected = chain.iter().enumerate().find_map(|(index, raw)| {
                    let entry = FallbackChainEntry::from_value(raw)?;
                    let model = entry.model.as_deref()?;
                    let candidate = BackendIdentity::new(
                        &entry.provider,
                        model,
                        entry.base_url.as_deref().unwrap_or(""),
                    );
                    let context = match model {
                        "small-8k-model" => Some(8_192),
                        "large-128k-model" => Some(131_072),
                        "unknown-context-model" => None,
                        _ => Some(256_000),
                    };
                    (!should_skip_candidate(&candidate, &failed, scope)
                        && context.is_none_or(|tokens| tokens >= 64_000))
                    .then_some((index, model.to_owned()))
                });
                assert_eq!(
                    selected.is_some(),
                    case["success"].as_bool().unwrap(),
                    "{case}"
                );
                assert_eq!(
                    selected.as_ref().map(|(_, model)| model.as_str()),
                    case["resolved_model"].as_str(),
                    "{case}"
                );
                if let Some((index, _)) = &selected {
                    assert!(
                        case["resolved_label"]
                            .as_str()
                            .unwrap()
                            .starts_with(&format!("fallback_chain[{index}]")),
                        "{case}"
                    );
                }
                continue;
            }
            let main = BackendIdentity::new(
                case["main_provider"].as_str().unwrap_or(""),
                case["main_model"].as_str().unwrap_or(""),
                "",
            );
            assert_eq!(
                !should_skip_candidate(&main, &failed, scope),
                case["success"].as_bool().unwrap(),
                "{case}"
            );
        }
    }

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
