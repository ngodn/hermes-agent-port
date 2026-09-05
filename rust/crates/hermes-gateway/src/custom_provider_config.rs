//! Read-only compatibility view for legacy and keyed provider configuration.
//! Never persist this derived view: doing so duplicates providers in the UI.
use crate::python_value::{python_repr, python_whitespace, truthy};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

fn text(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| python_repr(value))
        .trim_matches(python_whitespace)
        .into()
}
fn nonblank(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(|s| s.trim_matches(python_whitespace))
        .filter(|s| !s.is_empty())
}
fn alias<'a>(entry: &'a Map<String, Value>, first: &str, second: &str) -> &'a Value {
    entry
        .get(first)
        .filter(|value| truthy(value))
        .or_else(|| entry.get(second))
        .unwrap_or(&Value::Null)
}
fn api_mode(value: &str) -> &str {
    match value.to_lowercase().as_str() {
        "openai" | "openai_chat" | "openai-chat" | "chat-completions" | "chatcompletions" => {
            "chat_completions"
        }
        "responses" | "openai_responses" | "openai-responses" => "codex_responses",
        "anthropic" | "anthropic-messages" | "messages" => "anthropic_messages",
        "bedrock" | "bedrock-converse" => "bedrock_converse",
        _ => value,
    }
}

fn enabled(entry: &Value) -> bool {
    entry.get("enabled").is_none_or(|flag| match flag {
        Value::String(s) => !matches!(
            s.trim_matches(python_whitespace).to_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        ),
        _ => truthy(flag),
    })
}

/// Preserve route distinctions that URL clients often erase (path case, empty
/// query delimiters, repeated slashes and IPv6 zone case). Malformed input keeps
/// its literal identity, matching the Python route-normalization boundary.
fn route_identity(raw: &str) -> String {
    if raw.chars().any(|c| c <= ' ') {
        return raw.into();
    }
    let Some((scheme, rest)) = raw.split_once(':') else {
        return raw.into();
    };
    if !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return raw.into();
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return raw.into();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let mut host = crate::local_probe::urlparse_hostname(raw);
    if host.is_empty() {
        return raw.into();
    }
    let hostinfo = authority.rsplit('@').next().unwrap_or("");
    let port = if hostinfo.starts_with('[') {
        hostinfo
            .split_once(']')
            .and_then(|(_, tail)| tail.strip_prefix(':'))
    } else {
        hostinfo.split_once(':').map(|(_, port)| port)
    };
    let port = match port.filter(|p| !p.is_empty()) {
        Some(port) if port.bytes().all(|c| c.is_ascii_digit()) => match port.parse::<u16>() {
            Ok(port) => Some(port),
            Err(_) => return raw.into(),
        },
        Some(_) => return raw.into(),
        None => None,
    };
    let scheme = scheme.to_ascii_lowercase();
    if hostinfo.starts_with('[') || host.contains(':') {
        host = format!("[{host}]");
    }
    if let Some(port) =
        port.filter(|port| !matches!((scheme.as_str(), port), ("http", 80) | ("https", 443)))
    {
        host.push_str(&format!(":{port}"));
    }
    if let Some((userinfo, _)) = authority.rsplit_once('@') {
        host = format!("{userinfo}@{host}");
    }
    let suffix = rest[end..].split('#').next().unwrap_or("");
    let (path, query) = match suffix.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (suffix.strip_suffix('/').unwrap_or(suffix), None),
    };
    let mut result = format!("{scheme}://{host}{path}");
    if let Some(query) = query {
        result.push('?');
        result.push_str(query);
    }
    result
}

/// Headers belong to the effective endpoint. Skip matching entries without
/// headers, preserving the source's first nonempty match and legacy precedence.
pub fn extra_headers(config: &Value, base_url: &str) -> Map<String, Value> {
    if base_url.is_empty() {
        return Map::new();
    }
    let target = route_identity(base_url);
    for entry in compatible(config).as_array().into_iter().flatten() {
        let base = entry["base_url"].as_str().unwrap_or("");
        if !base.is_empty() && route_identity(base) == target {
            if let Some(headers) = entry["extra_headers"].as_object().filter(|h| !h.is_empty()) {
                return headers.clone();
            }
        }
    }
    Map::new()
}

fn runtime_aliases(name: &str, key: &str) -> HashSet<String> {
    let mut aliases = HashSet::new();
    for value in [name, key] {
        let raw = value.trim_matches(python_whitespace).to_lowercase();
        if raw.is_empty() {
            continue;
        }
        let normalized = raw.replace(' ', "-");
        aliases.insert(raw);
        aliases.insert(normalized.clone());
        if let Some(suffix) = normalized.strip_prefix("custom:") {
            if !suffix.is_empty() {
                aliases.insert(suffix.into());
                aliases.insert(format!("custom:{normalized}"));
            }
        } else {
            aliases.insert(format!("custom:{normalized}"));
        }
    }
    aliases
}

/// Named runtime lookup has keyed-first precedence, unlike the merged display
/// view. The caller supplies canonical built-in resolution and scoped env reads.
pub fn named(
    config: &Value,
    requested: &str,
    canonical_builtin: Option<&str>,
    mut get_env: impl FnMut(&str) -> String,
) -> Option<Value> {
    let requested = requested
        .trim_matches(python_whitespace)
        .to_lowercase()
        .replace(' ', "-");
    if requested.is_empty() || requested == "auto" {
        return None;
    }
    if requested != "custom"
        && !requested.starts_with("custom:")
        && canonical_builtin
            .is_some_and(|name| name.trim_matches(python_whitespace).to_lowercase() == requested)
    {
        return None;
    }
    if let Some(providers) = config["providers"].as_object() {
        for (key, entry) in providers {
            let Some(map) = entry.as_object() else {
                continue;
            };
            if !enabled(entry) {
                continue;
            }
            // Python applies `or ""` before string conversion. False and zero
            // must not become environment variable names.
            let env_value = alias(map, "key_env", "api_key_env");
            let env = if truthy(env_value) {
                text(env_value)
            } else {
                String::new()
            };
            let mut api_key = if env.is_empty() {
                String::new()
            } else {
                get_env(&env).trim_matches(python_whitespace).to_owned()
            };
            if api_key.is_empty() {
                api_key = if truthy(&entry["api_key"]) {
                    text(&entry["api_key"])
                } else {
                    String::new()
                };
            }
            let display = if truthy(&entry["name"]) {
                text(&entry["name"])
            } else {
                key.clone()
            };
            if !runtime_aliases(&display, key).contains(&requested) {
                continue;
            }
            let base = ["api", "url", "base_url"]
                .iter()
                .map(|key| &entry[*key])
                .find(|value| truthy(value));
            let Some(base) = base.and_then(Value::as_str) else {
                continue;
            };
            let mut result = json!({"name":entry.get("name").unwrap_or(&json!(key)),"base_url":base.trim_matches(python_whitespace),"api_key":api_key,"model":entry.get("default_model").unwrap_or(&json!(""))});
            if !key.trim_matches(python_whitespace).is_empty() {
                result["provider_key"] = json!(key.trim_matches(python_whitespace));
            }
            if !env.is_empty() {
                result["key_env"] = json!(env);
            }
            lift_runtime_fields(entry, &mut result, alias(map, "api_mode", "transport"));
            if truthy(&entry["key_cmd"]) {
                let command = text(&entry["key_cmd"]);
                if !command.is_empty() {
                    result["key_cmd"] = json!(command);
                }
            }
            return Some(result);
        }
    }
    for entry in compatible(config).as_array()? {
        let name = entry["name"].as_str()?;
        let key = text(&entry["provider_key"]);
        if !runtime_aliases(name, &key).contains(&requested) {
            continue;
        }
        let mut result = json!({"name":name.trim_matches(python_whitespace),"base_url":text(&entry["base_url"]),"api_key":text(&entry["api_key"])});
        for field in ["key_env", "provider_key", "model"] {
            let value = text(&entry[field]);
            if !value.is_empty() {
                result[field] = json!(value);
            }
        }
        lift_runtime_fields(entry, &mut result, &entry["api_mode"]);
        return Some(result);
    }
    None
}

fn lift_runtime_fields(entry: &Value, result: &mut Value, mode: &Value) {
    if entry["extra_body"].is_object() {
        result["extra_body"] = entry["extra_body"].clone();
    }
    if let Some(headers) = entry["extra_headers"].as_object() {
        let headers: Map<_, _> = headers
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| {
                (
                    k.clone(),
                    json!(v
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| python_repr(v))),
                )
            })
            .collect();
        if !headers.is_empty() {
            result["extra_headers"] = Value::Object(headers);
        }
    }
    if let Some(mode) = mode.as_str() {
        let mode = api_mode(mode.trim_matches(python_whitespace)).to_lowercase();
        if matches!(
            mode.as_str(),
            "chat_completions"
                | "codex_responses"
                | "anthropic_messages"
                | "bedrock_converse"
                | "codex_app_server"
        ) {
            result["api_mode"] = json!(mode);
        }
    }
    for field in ["max_output_tokens", "max_tokens"] {
        let value = &entry[field];
        if *value == json!(true)
            || (value.is_i64() || value.is_u64()) && value.as_f64().is_some_and(|n| n > 0.0)
        {
            result["max_output_tokens"] = value.clone();
            break;
        }
    }
    if let Some(caps) = entry["capabilities"].as_object() {
        let caps: Map<_, _> = caps
            .iter()
            .filter(|(_, v)| v.is_boolean())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !caps.is_empty() {
            result["capabilities"] = Value::Object(caps);
        }
    }
}
fn has_url_parts(value: &str) -> bool {
    if value
        .match_indices('{')
        .any(|(start, _)| value[start + 1..].find('}').is_some_and(|end| end > 0))
    {
        return true;
    }
    let value = value
        .trim_start_matches(|c: char| c <= ' ')
        .replace(['\t', '\r', '\n'], "");
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && rest
            .strip_prefix("//")
            .is_some_and(|rest| !rest.split(['/', '?', '#']).next().unwrap_or("").is_empty())
}

fn normalize(entry: &Value, provider_key: &str) -> Option<Value> {
    let provider_key = provider_key.trim_matches(python_whitespace);
    let mut entry = entry.as_object()?.clone();
    if !entry.contains_key("key_env") {
        if let Some(value) = entry.get("api_key_env").cloned() {
            entry.insert("key_env".into(), value);
        }
    }
    for (camel, snake) in [
        ("apiKey", "api_key"),
        ("baseUrl", "base_url"),
        ("apiMode", "api_mode"),
        ("keyEnv", "key_env"),
        ("apiKeyEnv", "key_env"),
        ("defaultModel", "default_model"),
        ("contextLength", "context_length"),
        ("rateLimitDelay", "rate_limit_delay"),
    ] {
        if !entry.contains_key(snake) {
            if let Some(value) = entry.get(camel).cloned() {
                entry.insert(snake.into(), value);
            }
        }
    }
    let base = ["base_url", "url", "api"]
        .iter()
        .filter_map(|key| entry.get(*key).and_then(nonblank))
        .find(|url| has_url_parts(url))?;
    let name = text(entry.get("name").unwrap_or(&Value::Null));
    let name = if name.is_empty() {
        provider_key.trim_matches(python_whitespace).to_owned()
    } else {
        name
    };
    if name.is_empty() {
        return None;
    }
    let mut out = Map::from_iter([
        ("name".into(), json!(name)),
        ("base_url".into(), json!(base)),
    ]);
    if !provider_key.is_empty() {
        out.insert(
            "provider_key".into(),
            json!(provider_key.trim_matches(python_whitespace)),
        );
    }
    for key in ["api_key", "ssl_ca_cert"] {
        if let Some(value) = entry.get(key).and_then(nonblank) {
            out.insert(key.into(), json!(value));
        }
    }
    if let Some(value) = nonblank(alias(&entry, "key_env", "api_key_env")) {
        out.insert("key_env".into(), json!(value));
        if entry.get("api_key_env").is_some_and(truthy) && !entry.get("key_env").is_some_and(truthy)
        {
            out.insert("api_key_env".into(), json!(value));
        }
    }
    if let Some(value) = nonblank(alias(&entry, "api_mode", "transport")) {
        out.insert("api_mode".into(), json!(api_mode(value)));
    }
    if let Some(value) = nonblank(alias(&entry, "model", "default_model")) {
        out.insert("model".into(), json!(value));
    }
    let mut discovered = entry.get("models_discovered") == Some(&json!(true));
    let mut models = Map::new();
    if let Some(mapping) = entry.get("models").and_then(Value::as_object) {
        models = mapping.clone();
        discovered |= models.remove("__discovered_model_catalog__") == Some(json!(true));
        models.remove("__explicit_model_allowlist__");
    } else if let Some(list) = entry.get("models").and_then(Value::as_array) {
        for item in list {
            if let Some(id) = nonblank(item) {
                models.insert(id.into(), json!({}));
            } else if let Some(mapping) = item.as_object() {
                if let Some(id) = mapping
                    .get("id")
                    .and_then(nonblank)
                    .or_else(|| mapping.get("name").and_then(nonblank))
                {
                    models.insert(
                        id.into(),
                        Value::Object(
                            mapping
                                .iter()
                                .filter(|(key, _)| !matches!(key.as_str(), "id" | "name"))
                                .map(|(key, value)| (key.clone(), value.clone()))
                                .collect(),
                        ),
                    );
                }
            }
        }
    }
    if !models.is_empty() {
        out.insert("models".into(), Value::Object(models));
    }
    if discovered {
        out.insert("models_discovered".into(), json!(true));
    }
    if let Some(capabilities) = entry.get("capabilities").and_then(Value::as_object) {
        let caps: Map<_, _> = capabilities
            .iter()
            .filter(|(_, v)| v.is_boolean())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !caps.is_empty() {
            out.insert("capabilities".into(), Value::Object(caps));
        }
    }
    if let Some(value) = entry.get("context_length").filter(|v| {
        **v == json!(true) || (v.is_i64() || v.is_u64()) && v.as_f64().is_some_and(|n| n > 0.0)
    }) {
        out.insert("context_length".into(), value.clone());
    }
    if let Some(value) = entry
        .get("rate_limit_delay")
        .filter(|v| v.is_boolean() || v.as_f64().is_some_and(|n| n >= 0.0))
    {
        out.insert("rate_limit_delay".into(), value.clone());
    }
    for key in ["discover_models", "ssl_verify"] {
        if let Some(value) = entry.get(key) {
            if value.is_boolean() {
                out.insert(key.into(), value.clone());
            } else if key == "ssl_verify" {
                if let Some(value) = nonblank(value) {
                    out.insert(key.into(), json!(value));
                }
            }
        }
    }
    if let Some(value) = entry.get("extra_body").filter(|v| v.is_object()) {
        out.insert("extra_body".into(), value.clone());
    }
    if let Some(headers) = entry.get("extra_headers").and_then(Value::as_object) {
        let headers: Map<_, _> = headers
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| {
                (
                    k.clone(),
                    json!(v
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| python_repr(v))),
                )
            })
            .collect();
        if !headers.is_empty() {
            out.insert("extra_headers".into(), Value::Object(headers));
        }
    }
    Some(Value::Object(out))
}

pub fn compatible(config: &Value) -> Value {
    let mut entries = vec![];
    if let Some(legacy) = config.get("custom_providers").filter(|v| !v.is_null()) {
        let Some(legacy) = legacy.as_array() else {
            return json!([]);
        };
        entries.extend(legacy.iter().filter_map(|entry| normalize(entry, "")));
    }
    if let Some(providers) = config["providers"].as_object() {
        for (key, entry) in providers {
            if enabled(entry) {
                if let Some(entry) = normalize(entry, key) {
                    entries.push(entry);
                }
            }
        }
    }
    let mut keys = HashSet::new();
    let mut pairs = HashSet::new();
    entries.retain(|entry| {
        let key = text(&entry["provider_key"]).to_lowercase();
        let pair = (
            text(&entry["name"]).to_lowercase(),
            text(&entry["base_url"])
                .trim_end_matches('/')
                .to_lowercase(),
            text(&entry["model"]).to_lowercase(),
        );
        if !key.is_empty() && keys.contains(&key)
            || !pair.0.is_empty() && !pair.1.is_empty() && pairs.contains(&pair)
        {
            return false;
        }
        if !key.is_empty() {
            keys.insert(key);
        }
        if !pair.0.is_empty() && !pair.1.is_empty() {
            pairs.insert(pair);
        }
        true
    });
    Value::Array(entries)
}

#[cfg(test)]
mod tests {
    #[test]
    fn route_identity_and_header_selection_match_python() {
        let rows: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/provider-route-headers-goldens.json"
        ))
        .unwrap();
        for row in rows["routes"].as_array().unwrap() {
            assert_eq!(
                super::route_identity(row["url"].as_str().unwrap()),
                row["result"],
                "{row}"
            );
        }
        for row in rows["headers"].as_array().unwrap() {
            assert_eq!(
                serde_json::json!(super::extra_headers(
                    &row["config"],
                    row["url"].as_str().unwrap()
                )),
                row["result"],
                "{row}"
            );
        }
    }
    #[test]
    fn named_provider_resolution_matches_python() {
        let rows: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/named-provider-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let mut calls = vec![];
            let result = super::named(
                &row["config"],
                row["requested"].as_str().unwrap(),
                row["canonical"].as_str(),
                |name| {
                    calls.push(name.to_owned());
                    row["env_key"].as_str().unwrap().into()
                },
            );
            assert_eq!(
                result.unwrap_or(serde_json::Value::Null),
                row["result"],
                "{row}"
            );
            assert_eq!(serde_json::json!(calls), row["calls"], "{row}");
        }
    }
    #[test]
    fn merged_provider_view_matches_python_without_mutation() {
        let rows: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/custom-provider-config-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let config = row["config"].clone();
            assert_eq!(super::compatible(&config), row["result"], "{row}");
            assert_eq!(config, row["config"]);
        }
    }
}
