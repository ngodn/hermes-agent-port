//! Provider registration and model-prefix recognition.
//! Registration consumes actual profiles, never plugin directory or manifest
//! names. Discovery and provider-specific transport hooks are separate work.
#![allow(dead_code)]
use crate::python_value::python_whitespace;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, RwLock},
};

/// Python distinguishes an inherited temperature from omission of the field.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Temperature {
    #[default]
    Inherit,
    Omit,
    Fixed(Value),
}

/// Native implementation attached at registration, independent of aliases or
/// later name changes. Base definitions have no request hook.
#[derive(Debug, Clone, Default)]
pub enum RequestHook {
    #[default]
    Base,
    Upstage,
    Nebius,
    Vercel,
}

/// Python profiles return separate SDK keyword and extra-body maps. Keep the
/// separation until caller overrides have been applied at the wire boundary.
#[derive(Debug, Default)]
pub struct RequestExtras {
    pub extra_body: serde_json::Map<String, Value>,
    pub top_level: serde_json::Map<String, Value>,
}

/// Declarative fields from providers/base.py. These describe transport choices;
/// they do not construct a client or replace provider-specific hook behavior.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    #[serde(skip)]
    pub request_hook: RequestHook,
    pub name: String,
    pub aliases: Vec<String>,
    pub api_mode: String,
    pub display_name: String,
    pub description: String,
    pub signup_url: String,
    pub env_vars: Vec<String>,
    pub base_url: String,
    pub models_url: String,
    pub auth_type: String,
    pub supports_health_check: bool,
    pub supports_vision: bool,
    pub supports_vision_tool_messages: bool,
    pub supports_prompt_cache_key: bool,
    pub process_command: String,
    pub process_args: Vec<String>,
    pub process_command_env_vars: Vec<String>,
    pub process_args_env_var: String,
    pub fallback_models: Vec<Value>,
    pub hostname: String,
    pub default_headers: serde_json::Map<String, Value>,
    pub fixed_temperature: Temperature,
    pub default_max_tokens: Option<i64>,
    pub default_aux_model: String,
}

impl ProviderProfile {
    /// Project provider-specific fields into the chat request. Each hook keeps
    /// its own disable, default and model-selection rules.
    pub fn api_kwargs_extras(
        &self,
        model: &str,
        config: Option<&Value>,
        supports_reasoning: bool,
    ) -> Result<RequestExtras, &'static str> {
        let mut extras = serde_json::Map::new();
        if matches!(self.request_hook, RequestHook::Vercel) {
            let mut result = RequestExtras::default();
            if supports_reasoning {
                let reasoning = match config.filter(|v| !v.is_null()) {
                    None => serde_json::json!({"enabled": true, "effort": "medium"}),
                    Some(Value::Object(map)) => Value::Object(map.clone()),
                    Some(Value::Array(pairs)) => {
                        let mut map = serde_json::Map::new();
                        for pair in pairs {
                            let pair = match pair {
                                Value::Array(pair) => pair.clone(),
                                Value::String(pair) => {
                                    pair.chars().map(|c| Value::String(c.to_string())).collect()
                                }
                                _ => return Err("invalid reasoning mapping entry"),
                            };
                            if pair.len() != 2 {
                                return Err("invalid reasoning mapping entry length");
                            }
                            let key = pair[0]
                                .as_str()
                                .ok_or("reasoning mapping key must be a string")?;
                            map.insert(key.to_owned(), pair[1].clone());
                        }
                        Value::Object(map)
                    }
                    Some(Value::String(value)) if value.is_empty() => {
                        Value::Object(Default::default())
                    }
                    _ => return Err("reasoning config must be a mapping"),
                };
                result.extra_body.insert("reasoning".into(), reasoning);
            }
            return Ok(result);
        }

        if matches!(self.request_hook, RequestHook::Base) {
            return Ok(RequestExtras {
                top_level: extras,
                ..RequestExtras::default()
            });
        }
        if matches!(self.request_hook, RequestHook::Nebius) {
            let model = model
                .trim_matches(python_whitespace)
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_lowercase();
            let model_supports = [
                "deepseek-r1",
                "deepseek-v4",
                "deepseek-reasoner",
                "gpt-oss",
                "glm-5",
                "kimi-k2",
                "minimax-m2",
                "qwen3",
            ]
            .iter()
            .any(|marker| model.contains(marker));
            if !supports_reasoning && !model_supports {
                return Ok(RequestExtras {
                    top_level: extras,
                    ..RequestExtras::default()
                });
            }
            let config = config.and_then(Value::as_object);
            if config.and_then(|c| c.get("enabled")) == Some(&Value::Bool(false)) {
                return Ok(RequestExtras {
                    top_level: extras,
                    ..RequestExtras::default()
                });
            }
            let raw = config
                .and_then(|c| c.get("effort"))
                .filter(|v| crate::python_value::truthy(v));
            let effort = raw
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| crate::python_value::python_repr(v))
                })
                .unwrap_or_else(|| "medium".into())
                .trim_matches(python_whitespace)
                .to_lowercase();
            if ["none", "off", "disabled"].contains(&effort.as_str()) {
                return Ok(RequestExtras {
                    top_level: extras,
                    ..RequestExtras::default()
                });
            }
            let supported = ["low", "medium", "high"].map(str::to_owned);
            let effort = crate::reasoning_effort::clamp_effort(Some(&effort), &supported, None)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "medium".into());
            extras.insert("reasoning_effort".into(), Value::String(effort));
            return Ok(RequestExtras {
                top_level: extras,
                ..RequestExtras::default()
            });
        }
        let model = model.trim_matches(python_whitespace).to_lowercase();
        if ["solar-mini", "syn-pro"]
            .iter()
            .any(|marker| model.contains(marker))
        {
            return Ok(RequestExtras {
                top_level: extras,
                ..RequestExtras::default()
            });
        }
        let config = config.and_then(Value::as_object);
        let effort = if let Some(config) = config.filter(|c| !c.is_empty()) {
            if config.get("enabled") == Some(&Value::Bool(false)) {
                return Ok(RequestExtras {
                    top_level: extras,
                    ..RequestExtras::default()
                });
            }
            let value = config
                .get("effort")
                .filter(|v| crate::python_value::truthy(v));
            let effort = match value {
                None => String::new(),
                Some(Value::String(value)) => value.trim_matches(python_whitespace).to_lowercase(),
                _ => return Err("Upstage reasoning effort must be a string"),
            };
            if effort == "minimal" {
                return Ok(RequestExtras {
                    top_level: extras,
                    ..RequestExtras::default()
                });
            }
            if effort.is_empty() {
                "medium".to_owned()
            } else {
                let supported = ["low", "medium", "high"].map(str::to_owned);
                let mapped =
                    crate::reasoning_effort::clamp_effort(Some(&effort), &supported, None).unwrap();
                if supported.contains(&mapped) {
                    mapped
                } else {
                    "high".into()
                }
            }
        } else {
            "medium".to_owned()
        };
        extras.insert("reasoning_effort".into(), Value::String(effort));
        Ok(RequestExtras {
            top_level: extras,
            ..RequestExtras::default()
        })
    }

    pub fn new(name: impl Into<String>) -> Self {
        Self {
            request_hook: RequestHook::Base,
            name: name.into(),
            aliases: vec![],
            api_mode: "chat_completions".into(),
            display_name: String::new(),
            description: String::new(),
            signup_url: String::new(),
            env_vars: vec![],
            base_url: String::new(),
            models_url: String::new(),
            auth_type: "api_key".into(),
            supports_health_check: true,
            supports_vision: false,
            supports_vision_tool_messages: true,
            supports_prompt_cache_key: false,
            process_command: String::new(),
            process_args: vec![],
            process_command_env_vars: vec![],
            process_args_env_var: String::new(),
            fallback_models: vec![],
            hostname: String::new(),
            default_headers: serde_json::Map::new(),
            fixed_temperature: Temperature::Inherit,
            default_max_tokens: None,
            default_aux_model: String::new(),
        }
    }
}

impl ProviderProfile {
    pub fn get_hostname(&self) -> String {
        if !self.hostname.is_empty() {
            self.hostname.clone()
        } else {
            crate::local_probe::urlparse_hostname(&self.base_url)
        }
    }

    /// An explicitly customized inference URL wins over a separate catalog URL.
    /// Echoing the profile's default back to us must not mask `models_url`.
    fn model_catalog_url(&self, caller_base: Option<&str>) -> Option<String> {
        let caller = caller_base.unwrap_or("").trim_matches(python_whitespace);
        let effective = if caller.is_empty() {
            &self.base_url
        } else {
            caller
        };
        if !caller.is_empty() && caller.trim_end_matches('/') != self.base_url.trim_end_matches('/')
        {
            return Some(format!("{}/models", caller.trim_end_matches('/')));
        }
        let catalog = self.models_url.trim_matches(python_whitespace);
        if !catalog.is_empty() {
            return Some(catalog.into());
        }
        if effective.is_empty() {
            return None;
        }
        Some(format!("{}/models", effective.trim_end_matches('/')))
    }

    /// Base profile's real model-list request. The caller supplies its versioned
    /// Hermes User-Agent. Provider headers override the generic defaults.
    pub async fn fetch_models(
        &self,
        api_key: Option<&str>,
        base_url: Option<&str>,
        timeout: std::time::Duration,
        user_agent: &str,
    ) -> Option<Vec<Value>> {
        let bundle = ca_bundle_path(|name| std::env::var(name).ok());
        self.fetch_models_with_ca(api_key, base_url, timeout, user_agent, bundle.as_deref())
            .await
    }

    async fn fetch_models_with_ca(
        &self,
        api_key: Option<&str>,
        base_url: Option<&str>,
        timeout: std::time::Duration,
        user_agent: &str,
        ca_bundle: Option<&std::path::Path>,
    ) -> Option<Vec<Value>> {
        use reqwest::header::{
            HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT,
        };
        let endpoint = self.model_catalog_url(base_url)?;
        let mut url = reqwest::Url::parse(endpoint.trim_matches(python_whitespace)).ok()?;
        let original = origin(&url);
        let mut headers = HeaderMap::new();
        if let Some(key) = api_key.filter(|key| !key.is_empty()) {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {key}")).ok()?,
            );
        }
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(USER_AGENT, HeaderValue::from_str(user_agent).ok()?);
        for (name, value) in &self.default_headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).ok()?,
                HeaderValue::from_str(value.as_str()?).ok()?,
            );
        }
        let client = profile_http_client(timeout, ca_bundle)?;
        let mut visited = HashMap::<String, usize>::new();
        loop {
            // The Python guard compares every hop to the original origin.
            // Once removed, credentials are never restored, even on return.
            if origin(&url) != original {
                let mut safe = HeaderMap::new();
                for key in [ACCEPT, USER_AGENT] {
                    if let Some(value) = headers.get(&key) {
                        safe.insert(key, value.clone());
                    }
                }
                headers = safe;
            }
            let response = client
                .get(url.clone())
                .headers(headers.clone())
                .send()
                .await
                .ok()?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get("location")
                    .or_else(|| response.headers().get("uri"))?
                    .to_str()
                    .ok()?;
                let target = url.join(location).ok()?;
                if !matches!(target.scheme(), "http" | "https") {
                    return None;
                }
                let key = target.as_str().to_owned();
                if visited.get(&key).copied().unwrap_or(0) >= 4 || visited.len() >= 10 {
                    return None;
                }
                *visited.entry(key).or_default() += 1;
                url = target;
                continue;
            }
            if !response.status().is_success() {
                return None;
            }
            return model_ids(&response.json::<Value>().await.ok()?);
        }
    }
}

/// The first nonblank variable wins, even when its file is unusable. Python
/// falls back to default roots in that case, not to the next environment key.
fn ca_bundle_path(mut env: impl FnMut(&str) -> Option<String>) -> Option<std::path::PathBuf> {
    let raw = [
        "HERMES_CA_BUNDLE",
        "SSL_CERT_FILE",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
    ]
    .into_iter()
    .find_map(|key| {
        env(key)
            .map(|value| value.trim_matches(python_whitespace).to_owned())
            .filter(|value| !value.is_empty())
    })?;
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = env("HOME") {
            return Some(std::path::PathBuf::from(home).join(raw.strip_prefix("~/").unwrap_or("")));
        }
    }
    Some(raw.into())
}

fn profile_http_client(
    timeout: std::time::Duration,
    ca_bundle: Option<&std::path::Path>,
) -> Option<reqwest::Client> {
    let builder = || {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout)
            .read_timeout(timeout)
    };
    if let Some(path) = ca_bundle {
        let custom = (|| {
            let bytes = std::fs::read(path).ok()?;
            let certificates = reqwest::Certificate::from_pem_bundle(&bytes).ok()?;
            if certificates.is_empty() {
                return None;
            }
            // ssl.create_default_context(cafile=...) uses that trust store,
            // rather than silently augmenting it with the default roots.
            let mut client = builder().tls_built_in_root_certs(false);
            for certificate in certificates {
                client = client.add_root_certificate(certificate);
            }
            client.build().ok()
        })();
        if let Some(client) = custom {
            return Some(client);
        }
        tracing::warn!(
            "Provider CA bundle could not be loaded; falling back to default certificates"
        );
    }
    builder().build().ok()
}

fn origin(url: &reqwest::Url) -> (String, String, Option<u16>) {
    (
        url.scheme().to_lowercase(),
        url.host_str()
            .unwrap_or("")
            .to_lowercase()
            .trim_end_matches('.')
            .to_owned(),
        url.port_or_known_default(),
    )
}

fn model_ids(data: &Value) -> Option<Vec<Value>> {
    let items = match data {
        Value::Array(items) => items.as_slice(),
        Value::Object(object) => match object.get("data") {
            None => return Some(vec![]),
            Some(Value::Array(items)) => items.as_slice(),
            // Python iterates dict/string collections but finds no model dicts.
            Some(Value::Object(_)) | Some(Value::String(_)) => return Some(vec![]),
            _ => return None,
        },
        _ => return None,
    };
    Some(
        items
            .iter()
            .filter_map(|item| item.as_object()?.get("id").cloned())
            .collect(),
    )
}

pub type SharedProfile = Arc<RwLock<ProviderProfile>>;

#[derive(Default)]
struct RegistryState {
    // Canonical replacements retain insertion position, matching Python dicts.
    profiles: Vec<(String, SharedProfile)>,
    aliases: HashMap<String, String>,
    list_cache: Option<Vec<SharedProfile>>,
}

#[derive(Default)]
pub struct ProviderRegistry {
    state: RwLock<RegistryState>,
}

impl ProviderRegistry {
    /// Register bundled modules whose behavior is entirely the base profile.
    /// This is a native loader for those definitions. It does not discover or
    /// execute user plugins, nor claim hooks for other bundled modules.
    pub fn register_bundled_base_profiles(&self, hermes_version: &str) -> Vec<String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Module {
            module: String,
            profiles: Vec<ProviderProfile>,
        }
        let modules: Vec<Module> =
            serde_json::from_str(include_str!("../../../tools/bundled-base-profiles.json"))
                .expect("valid embedded native provider definitions");
        let mut loaded = Vec::new();
        for module in modules {
            for mut profile in module.profiles {
                for value in profile.default_headers.values_mut() {
                    if let Value::String(header) = value {
                        *header = header.replace("__HERMES_NATIVE_VERSION__", hermes_version);
                    }
                }
                self.register(Arc::new(RwLock::new(profile)));
            }
            loaded.push(module.module);
        }
        loaded
    }

    /// This profile has a native implementation for every overridden hook.
    /// Keep it separate from the loader that accepts base-only declarations.
    pub fn register_upstage(&self) {
        let mut profile: ProviderProfile =
            serde_json::from_str(include_str!("../../../tools/upstage-profile.json"))
                .expect("valid embedded Upstage profile");
        profile.request_hook = RequestHook::Upstage;
        self.register(Arc::new(RwLock::new(profile)));
    }

    /// Nebius inherits the base catalog transport, including its verbose URL.
    pub fn register_nebius(&self) {
        let mut profile: ProviderProfile =
            serde_json::from_str(include_str!("../../../tools/nebius-profile.json"))
                .expect("valid embedded Nebius profile");
        profile.request_hook = RequestHook::Nebius;
        self.register(Arc::new(RwLock::new(profile)));
    }

    pub fn register_vercel(&self) {
        let mut profile: ProviderProfile =
            serde_json::from_str(include_str!("../../../tools/vercel-profile.json"))
                .expect("valid embedded Vercel profile");
        profile.request_hook = RequestHook::Vercel;
        self.register(Arc::new(RwLock::new(profile)));
    }

    pub fn register(&self, profile: SharedProfile) {
        let (name, aliases) = {
            let value = profile.read().unwrap();
            (value.name.clone(), value.aliases.clone())
        };
        let mut state = self.state.write().unwrap();
        if let Some((_, old)) = state.profiles.iter_mut().find(|(key, _)| key == &name) {
            *old = profile;
        } else {
            state.profiles.push((name.clone(), profile));
        }
        // Old aliases deliberately survive replacement; they resolve to the
        // current profile under their stored canonical name.
        for alias in aliases {
            state.aliases.insert(alias, name.clone());
        }
        state.list_cache = None;
    }

    pub fn get(&self, name: &str) -> Option<SharedProfile> {
        let state = self.state.read().unwrap();
        let canonical = state.aliases.get(name).map(String::as_str).unwrap_or(name);
        state
            .profiles
            .iter()
            .find(|(key, _)| key == canonical)
            .map(|(_, value)| Arc::clone(value))
    }

    pub fn list(&self) -> Vec<SharedProfile> {
        let mut state = self.state.write().unwrap();
        if let Some(cached) = &state.list_cache {
            return cached.clone();
        }
        let mut result = Vec::new();
        for (_, profile) in &state.profiles {
            if !result.iter().any(|other| Arc::ptr_eq(other, profile)) {
                result.push(Arc::clone(profile));
            }
        }
        state.list_cache = Some(result.clone());
        result
    }

    /// Only registered names or aliases count. Preserve the original suffix,
    /// including whitespace; an Ollama model tag must keep its prefix intact.
    pub fn strip_model_prefix<'a>(&self, model: &'a str) -> &'a str {
        if model.starts_with("http") {
            return model;
        }
        let Some((prefix, suffix)) = model.split_once(':') else {
            return model;
        };
        let prefix = prefix.trim_matches(python_whitespace).to_lowercase();
        if self.get(&prefix).is_none()
            || looks_like_ollama_tag(suffix.trim_matches(python_whitespace))
        {
            return model;
        }
        suffix
    }
}

fn looks_like_ollama_tag(suffix: &str) -> bool {
    static PATTERN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        fancy_regex::Regex::new(
            r"(?i)^([0-9]+\.?[0-9]*b|latest|stable|q[0-9]|fp?[0-9]|instruct|chat|coder|vision|text)",
        )
        .unwrap()
    });
    // Python re.IGNORECASE also treats dotted and dotless I as ASCII i.
    // Fold only the reference Python version's decimal digits, so newer Rust
    // Unicode tables do not misclassify a model name as an Ollama tag.
    let folded: String = suffix
        .chars()
        .map(|c| match c {
            '\u{130}' | '\u{131}' => 'i',
            c => crate::python_value::decimal_digit(c)
                .map(|digit| char::from(b'0' + digit as u8))
                .unwrap_or(c),
        })
        .collect();
    PATTERN.is_match(&folded).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #[test]
    fn vercel_hook_preserves_full_reasoning_config_and_gate() {
        let registry = super::ProviderRegistry::default();
        registry.register_vercel();
        let profile = registry.get("vercel").unwrap();
        let profile = profile.read().unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/vercel-goldens.json")).unwrap();
        for row in fixture.as_array().unwrap() {
            let result = profile.api_kwargs_extras(
                "",
                Some(&row["config"]),
                row["supports"].as_bool().unwrap(),
            );
            if row.get("error").is_some() {
                assert!(result.is_err(), "{row}");
            } else {
                let result = result.unwrap();
                assert_eq!(
                    serde_json::json!([result.extra_body, result.top_level]),
                    row["result"],
                    "{row}"
                );
            }
        }
    }

    #[test]
    fn nebius_profile_and_hook_match_python() {
        let registry = super::ProviderRegistry::default();
        registry.register_nebius();
        let profile = registry.get("nebius").unwrap();
        let profile = profile.read().unwrap();
        let definition: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/nebius-profile.json")).unwrap();
        assert_eq!(serde_json::to_value(&*profile).unwrap(), definition);
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/nebius-goldens.json")).unwrap();
        for row in fixture.as_array().unwrap() {
            let result = profile
                .api_kwargs_extras(
                    row["model"].as_str().unwrap_or(""),
                    Some(&row["config"]),
                    row["supports"].as_bool().unwrap(),
                )
                .unwrap();
            assert_eq!(
                serde_json::Value::Object(result.top_level),
                row["result"][1],
                "{row}"
            );
        }
    }

    #[test]
    fn upstage_hook_matches_python_including_malformed_efforts() {
        let registry = super::ProviderRegistry::default();
        registry.register_upstage();
        let profile = registry.get("solar").unwrap();
        let profile = profile.read().unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/upstage-goldens.json")).unwrap();
        for row in fixture["hooks"].as_array().unwrap() {
            let result = profile.api_kwargs_extras(
                row["model"].as_str().unwrap_or(""),
                Some(&row["config"]),
                false,
            );
            if row.get("error").is_some() {
                assert!(result.is_err(), "{row}");
            } else {
                assert_eq!(
                    serde_json::Value::Object(result.unwrap().top_level),
                    row["result"][1],
                    "{row}"
                );
            }
        }
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn registration_and_prefixes_match_python() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tools/provider-registry-goldens.json"
        ))
        .unwrap();
        let registry = ProviderRegistry::default();
        let mut handles: HashMap<String, SharedProfile> = HashMap::new();
        let marker = |profile: &SharedProfile, handles: &HashMap<String, SharedProfile>| {
            handles
                .iter()
                .find(|(_, handle)| Arc::ptr_eq(profile, handle))
                .unwrap()
                .0
                .clone()
        };
        for record in fixture["trace"].as_array().unwrap() {
            let step = &record["step"];
            let key = step["key"].as_str().unwrap();
            let profile = handles
                .entry(key.into())
                .or_insert_with(|| Arc::new(RwLock::new(ProviderProfile::new(""))))
                .clone();
            {
                let mut profile = profile.write().unwrap();
                profile.name = step["name"].as_str().unwrap().into();
                profile.aliases = serde_json::from_value(step["aliases"].clone()).unwrap();
            }
            registry.register(profile);
            for (name, expected) in record["get"].as_object().unwrap() {
                assert_eq!(
                    json!(registry.get(name).map(|p| marker(&p, &handles))),
                    *expected,
                    "lookup {name} after {key}"
                );
            }
            assert_eq!(
                json!(registry
                    .list()
                    .iter()
                    .map(|p| marker(p, &handles))
                    .collect::<Vec<_>>()),
                record["listed"]
            );
        }
        registry.list().clear();
        assert_eq!(
            json!(registry
                .list()
                .iter()
                .map(|p| marker(p, &handles))
                .collect::<Vec<_>>()),
            fixture["final_list"]
        );
        let mut local = ProviderProfile::new("local");
        local.aliases = ["custom", "http", "deepseek", "qwen"]
            .map(str::to_owned)
            .to_vec();
        registry.register(Arc::new(RwLock::new(local)));
        for (index, case) in fixture["prefixes"].as_array().unwrap().iter().enumerate() {
            assert_eq!(
                registry.strip_model_prefix(case["model"].as_str().unwrap()),
                case["expected"].as_str().unwrap(),
                "prefix case {index}"
            );
        }
    }
    #[test]
    fn base_model_fetch_selection_and_shapes_match_python() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../tools/provider-fetch-goldens.json"))
                .unwrap();
        for (index, case) in fixture["fetch"].as_array().unwrap().iter().enumerate() {
            let mut profile = ProviderProfile::new("fixture");
            profile.base_url = case["base"].as_str().unwrap().into();
            profile.models_url = case["catalog"].as_str().unwrap().into();
            let url = profile.model_catalog_url(case["caller"].as_str());
            let expected_url = case["calls"]
                .as_array()
                .unwrap()
                .first()
                .map(|call| call["url"].as_str().unwrap());
            assert_eq!(
                url.as_deref().map(str::trim),
                expected_url,
                "URL case {index}"
            );
            let result = url.and_then(|_| model_ids(&case["body"]));
            assert_eq!(json!(result), case["expected"], "body case {index}");
        }
        for case in fixture["hostnames"].as_array().unwrap() {
            let mut profile = ProviderProfile::new("fixture");
            profile.base_url = case["base"].as_str().unwrap().into();
            profile.hostname = case["explicit"].as_str().unwrap().into();
            assert_eq!(profile.get_hostname(), case["expected"].as_str().unwrap());
        }
    }

    struct TestServer(String, tokio::task::JoinHandle<()>);
    impl Drop for TestServer {
        fn drop(&mut self) {
            self.1.abort();
        }
    }
    async fn server(app: axum::Router) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        TestServer(
            url,
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
        )
    }

    #[tokio::test]
    async fn custom_endpoint_and_headers_reach_real_http() {
        use axum::{
            http::{HeaderMap, Uri},
            Json, Router,
        };
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = received.clone();
        let live = server(Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
            let record = record.clone();
            async move {
                record
                    .lock()
                    .unwrap()
                    .push((uri.path().to_owned(), headers));
                Json(json!({"data": [{"id": "a"}, {"id": "a"}, {"id": null}, {"id": 7}]}))
            }
        }))
        .await;
        let mut profile = ProviderProfile::new("fixture");
        profile.base_url = format!("{}/inference", live.0);
        profile.models_url = format!("{}/catalog", live.0);
        profile.default_headers = serde_json::from_value(json!({"Authorization": "Custom override", "Accept": "custom/type", "User-Agent": "custom-agent", "X-Private": "secret"})).unwrap();
        assert_eq!(
            profile
                .fetch_models(
                    Some("key"),
                    Some(&format!("{}/proxy/", live.0)),
                    std::time::Duration::from_secs(2),
                    "hermes-cli/fixture"
                )
                .await,
            Some(vec![json!("a"), json!("a"), Value::Null, json!(7)])
        );
        assert!(profile
            .fetch_models(
                None,
                Some(&profile.base_url),
                std::time::Duration::from_secs(2),
                "hermes-cli/fixture"
            )
            .await
            .is_some());
        let calls = received.lock().unwrap();
        assert_eq!(calls[0].0, "/proxy/models");
        assert_eq!(calls[1].0, "/catalog");
        for (_, headers) in calls.iter() {
            assert_eq!(headers["authorization"], "Custom override");
            assert_eq!(headers["accept"], "custom/type");
            assert_eq!(headers["user-agent"], "custom-agent");
            assert_eq!(headers["x-private"], "secret");
        }
    }

    #[tokio::test]
    async fn redirects_drop_all_private_headers_and_do_not_restore_them() {
        use axum::{
            http::{HeaderMap, StatusCode, Uri},
            response::IntoResponse,
            Json, Router,
        };
        let recorded = Arc::new(std::sync::Mutex::new(Vec::<(String, HeaderMap)>::new()));
        let return_url = Arc::new(std::sync::Mutex::new(String::new()));
        let second_record = recorded.clone();
        let second_return = return_url.clone();
        let second = server(Router::new().fallback(move |headers: HeaderMap| {
            let record = second_record.clone();
            let target = second_return.clone();
            async move {
                record.lock().unwrap().push(("cross".into(), headers));
                (
                    StatusCode::FOUND,
                    [("location", target.lock().unwrap().clone())],
                )
                    .into_response()
            }
        }))
        .await;
        let first_record = recorded.clone();
        let cross_url = second.0.clone();
        let first = server(Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
            let record = first_record.clone();
            let cross = cross_url.clone();
            async move {
                record.lock().unwrap().push((uri.path().into(), headers));
                match uri.path() {
                    "/models" => {
                        (StatusCode::FOUND, [("location", "/same".to_owned())]).into_response()
                    }
                    "/same" => {
                        (StatusCode::TEMPORARY_REDIRECT, [("location", cross)]).into_response()
                    }
                    _ => Json(json!({"data": [{"id": "redirected"}]})).into_response(),
                }
            }
        }))
        .await;
        *return_url.lock().unwrap() = format!("{}/final", first.0);
        let mut profile = ProviderProfile::new("fixture");
        profile.base_url = first.0.clone();
        profile.default_headers =
            serde_json::from_value(json!({"X-Secret": "private", "Cookie": "session=private"}))
                .unwrap();
        assert_eq!(
            profile
                .fetch_models(
                    Some("key"),
                    None,
                    std::time::Duration::from_secs(2),
                    "hermes-cli/fixture"
                )
                .await,
            Some(vec![json!("redirected")])
        );
        let calls = recorded.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>(),
            ["/models", "/same", "cross", "/final"]
        );
        for (index, (_, headers)) in calls.iter().enumerate() {
            assert_eq!(headers["accept"], "application/json");
            assert_eq!(headers["user-agent"], "hermes-cli/fixture");
            for secret in ["authorization", "x-secret", "cookie"] {
                assert_eq!(
                    headers.contains_key(secret),
                    index < 2,
                    "header {secret} on hop {index}"
                );
            }
        }
    }

    #[tokio::test]
    async fn redirect_cycles_and_invalid_payloads_fail_without_model_ids() {
        use axum::{
            http::{StatusCode, Uri},
            response::IntoResponse,
            Router,
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let live = server(Router::new().fallback(move |uri: Uri| {
            let count = count.clone();
            async move {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match uri.path() {
                    "/cycle" => (StatusCode::FOUND, [("location", "/cycle")]).into_response(),
                    "/error" => StatusCode::BAD_GATEWAY.into_response(),
                    _ => "invalid JSON".into_response(),
                }
            }
        }))
        .await;
        let mut profile = ProviderProfile::new("fixture");
        for path in ["cycle", "error", "invalid"] {
            profile.models_url = format!("{}/{path}", live.0);
            assert!(profile
                .fetch_models(
                    None,
                    None,
                    std::time::Duration::from_secs(2),
                    "hermes-cli/fixture"
                )
                .await
                .is_none());
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 7);
    }
    #[test]
    fn ca_environment_precedence_and_home_expansion() {
        let keys = [
            "HERMES_CA_BUNDLE",
            "SSL_CERT_FILE",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
        ];
        for start in 0..keys.len() {
            let env: HashMap<_, _> = keys
                .iter()
                .enumerate()
                .map(|(i, key)| {
                    (
                        *key,
                        if i < start {
                            "   ".into()
                        } else {
                            format!(" /ca/{i}.pem ")
                        },
                    )
                })
                .collect();
            assert_eq!(
                ca_bundle_path(|key| env.get(key).cloned()),
                Some(format!("/ca/{start}.pem").into())
            );
        }
        assert!(ca_bundle_path(|_| None).is_none());
        assert_eq!(
            ca_bundle_path(|key| match key {
                "HERMES_CA_BUNDLE" => Some(" ~/ca.pem ".into()),
                "HOME" => Some("/fixture/home".into()),
                _ => None,
            }),
            Some("/fixture/home/ca.pem".into())
        );
        assert_eq!(
            ca_bundle_path(|key| match key {
                "HERMES_CA_BUNDLE" => Some("/missing.pem".into()),
                "SSL_CERT_FILE" => Some("/valid.pem".into()),
                _ => None,
            }),
            Some("/missing.pem".into())
        );
    }

    #[test]
    fn custom_ca_controls_real_model_fetch_and_keeps_hostname_verification() {
        let _guard = crate::secret_scope::GLOBAL_TEST_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::{rustls, TlsAcceptor};
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(
                include_bytes!("../../../tools/tls-fixtures/server.der").to_vec(),
            )],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                include_bytes!("../../../tools/tls-fixtures/server-key.der").to_vec(),
            )),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(socket).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    loop {
                        let mut buffer = [0; 1024];
                        let Ok(count) = stream.read(&mut buffer).await else {
                            return;
                        };
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                        if request.len() > 16384 {
                            return;
                        }
                    }
                    let body = r#"{"data":[{"id":"private-model"}]}"#;
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        let root =
            std::env::temp_dir().join(format!("hermes-provider-ca-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        struct Cleanup(std::path::PathBuf, tokio::task::JoinHandle<()>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.1.abort();
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone(), task);
        let ca = root.join("ca.pem");
        let unrelated = root.join("other.pem");
        let bundle = root.join("bundle.pem");
        let invalid = root.join("invalid.pem");
        std::fs::write(&ca, include_bytes!("../../../tools/tls-fixtures/ca.pem")).unwrap();
        std::fs::write(
            &unrelated,
            include_bytes!("../../../tools/tls-fixtures/other-ca.pem"),
        )
        .unwrap();
        let mut both = std::fs::read(&unrelated).unwrap();
        both.extend(std::fs::read(&ca).unwrap());
        std::fs::write(&bundle, both).unwrap();
        std::fs::write(&invalid, b"not a certificate").unwrap();
        let timeout = std::time::Duration::from_secs(2);
        let mut profile = ProviderProfile::new("fixture");
        profile.base_url = format!("https://localhost:{port}");
        for path in [
            None,
            Some(unrelated.as_path()),
            Some(invalid.as_path()),
            Some(root.join("missing").as_path()),
        ] {
            assert!(
                profile_http_client(timeout, path).is_some(),
                "invalid bundles retain default client construction"
            );
            assert!(profile
                .fetch_models_with_ca(None, None, timeout, "hermes-cli/test", path)
                .await
                .is_none());
        }
        for path in [&ca, &bundle] {
            assert_eq!(
                profile
                    .fetch_models_with_ca(None, None, timeout, "hermes-cli/test", Some(path))
                    .await,
                Some(vec![json!("private-model")])
            );
        }
        // The chain is trusted but the IP is absent from the DNS-only SAN.
        profile.base_url = format!("https://127.0.0.1:{port}");
        assert!(profile
            .fetch_models_with_ca(None, None, timeout, "hermes-cli/test", Some(&ca))
            .await
            .is_none());
        profile.base_url = format!("https://localhost:{port}");
        struct EnvRestore(&'static str, Option<std::ffi::OsString>);
        impl Drop for EnvRestore { fn drop(&mut self) { match &self.1 { Some(value) => std::env::set_var(self.0, value), None => std::env::remove_var(self.0) } } }
        let _ca_env = EnvRestore("HERMES_CA_BUNDLE", std::env::var_os("HERMES_CA_BUNDLE"));
        let _ssl_env = EnvRestore("SSL_CERT_FILE", std::env::var_os("SSL_CERT_FILE"));
        std::env::set_var("HERMES_CA_BUNDLE", &ca);
        assert_eq!(profile.fetch_models(None, None, timeout, "hermes-cli/test").await, Some(vec![json!("private-model")]));
        std::env::set_var("HERMES_CA_BUNDLE", &invalid);
        std::env::set_var("SSL_CERT_FILE", &ca);
        assert!(profile.fetch_models(None, None, timeout, "hermes-cli/test").await.is_none(), "a bad first bundle must not select the second variable");
        // A parseable PEM block with invalid DER invalidates the whole bundle.
        let mut mixed = std::fs::read(&ca).unwrap();
        mixed.extend_from_slice(b"\n-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n");
        std::fs::write(&invalid, mixed).unwrap();
        assert!(profile_http_client(timeout, Some(&invalid)).is_some());
        assert!(profile.fetch_models_with_ca(None, None, timeout, "hermes-cli/test", Some(&invalid)).await.is_none());
        });
    }
    #[test]
    fn bundled_base_definitions_register_all_fields_and_allow_later_overrides() {
        let registry = ProviderRegistry::default();
        let loaded = registry.register_bundled_base_profiles("__HERMES_NATIVE_VERSION__");
        let modules: Value =
            serde_json::from_str(include_str!("../../../tools/bundled-base-profiles.json"))
                .unwrap();
        assert_eq!(
            loaded,
            modules
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["module"].as_str().unwrap())
                .collect::<Vec<_>>()
        );
        for module in modules.as_array().unwrap() {
            for expected in module["profiles"].as_array().unwrap() {
                let actual = registry.get(expected["name"].as_str().unwrap()).unwrap();
                assert_eq!(
                    serde_json::to_value(&*actual.read().unwrap()).unwrap(),
                    *expected
                );
                for alias in expected["aliases"].as_array().unwrap() {
                    assert!(Arc::ptr_eq(
                        &registry.get(alias.as_str().unwrap()).unwrap(),
                        &actual
                    ));
                }
            }
        }
        assert!(
            registry.get("custom").is_none(),
            "custom hooks are not part of the base-only loader"
        );
        let replacement = Arc::new(RwLock::new(ProviderProfile::new("gmi")));
        registry.register(replacement.clone());
        assert!(Arc::ptr_eq(
            &registry.get("gmi-cloud").unwrap(),
            &replacement
        ));
    }
}
