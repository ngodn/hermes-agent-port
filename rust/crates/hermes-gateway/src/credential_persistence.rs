//! Disk-boundary rules for borrowed credential references.
//! Decoding a row must not strip the live secret needed by the current turn.
#![allow(dead_code)]
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

fn text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::python_value::python_repr(value))
}

fn normalized(value: &Value) -> String {
    if !crate::python_value::truthy(value) {
        return String::new();
    }
    text(value)
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase()
}

pub fn is_borrowed(source: &Value, provider: &Value) -> bool {
    let source = normalized(source);
    if source.is_empty() || source == "manual" || source.starts_with("manual:") {
        return false;
    }
    !matches!(
        (normalized(provider).as_str(), source.as_str()),
        ("anthropic", "hermes_pkce")
            | ("minimax-oauth", "oauth")
            | ("nous" | "openai-codex" | "xai-oauth", "device_code")
    )
}

fn secret_key(key: &str) -> bool {
    let mut expanded = String::new();
    let mut previous = None;
    for c in key
        .trim_matches(crate::python_value::python_whitespace)
        .chars()
    {
        if c.is_ascii_uppercase()
            && previous.is_some_and(|p: char| p.is_ascii_lowercase() || p.is_ascii_digit())
        {
            expanded.push('_');
        }
        expanded.push(c);
        previous = Some(c);
    }
    let key = expanded.to_lowercase().replace(['-', '.'], "_");
    const SAFE: &[&str] = &[
        "secret_fingerprint",
        "secret_source",
        "token_type",
        "scope",
        "client_id",
        "agent_key_id",
        "agent_key_expires_at",
        "agent_key_expires_in",
        "agent_key_reused",
        "agent_key_obtained_at",
        "expires_at",
        "expires_at_ms",
        "expires_in",
        "last_refresh",
        "last_status",
        "last_status_at",
        "last_error_code",
        "last_error_reason",
        "last_error_message",
        "last_error_reset_at",
    ];
    const KEYS: &[&str] = &[
        "access_token",
        "refresh_token",
        "agent_key",
        "api_key",
        "apikey",
        "api_token",
        "auth_token",
        "authorization",
        "bearer_token",
        "client_secret",
        "credential",
        "credentials",
        "id_token",
        "oauth_token",
        "private_key",
        "secret_key",
        "session_token",
        "password",
        "secret",
        "token",
        "tokens",
    ];
    const SUFFIXES: &[&str] = &[
        "_api_key",
        "_api_token",
        "_access_token",
        "_auth_token",
        "_refresh_token",
        "_bearer_token",
        "_client_secret",
        "_id_token",
        "_oauth_token",
        "_private_key",
        "_session_token",
        "_secret_key",
        "_password",
        "_secret",
        "_token",
        "_key",
    ];
    !SAFE.contains(&key.as_str())
        && (KEYS.contains(&key.as_str()) || SUFFIXES.iter().any(|suffix| key.ends_with(suffix)))
}

pub fn fingerprint(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let text = text(value);
    if text.is_empty() {
        return None;
    }
    let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
    Some(format!("sha256:{}", &digest[..16]))
}

pub fn sanitize(payload: &Map<String, Value>, provider: &str) -> Map<String, Value> {
    if !is_borrowed(
        payload.get("source").unwrap_or(&Value::Null),
        &Value::String(provider.into()),
    ) {
        return payload.clone();
    }
    let fingerprint = [
        "agent_key",
        "access_token",
        "refresh_token",
        "api_key",
        "token",
        "secret",
    ]
    .iter()
    .filter_map(|key| payload.get(*key))
    .find_map(fingerprint)
    .or_else(|| {
        payload
            .iter()
            .filter(|(key, _)| secret_key(key))
            .find_map(|(_, value)| fingerprint(value))
    })
    .or_else(|| {
        payload
            .get("secret_fingerprint")
            .and_then(Value::as_str)
            .filter(|value| value.starts_with("sha256:"))
            .map(str::to_owned)
    });
    let mut sanitized: Map<String, Value> = payload
        .iter()
        .filter(|(key, _)| !secret_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(fingerprint) = fingerprint {
        sanitized.insert("secret_fingerprint".into(), Value::String(fingerprint));
    }
    sanitized
}

#[cfg(test)]
mod tests {
    #[test]
    fn disk_boundary_matches_python() {
        let rows: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/credential-persistence-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            assert_eq!(
                serde_json::Value::Object(super::sanitize(
                    row["payload"].as_object().unwrap(),
                    row["provider"].as_str().unwrap()
                )),
                row["result"],
                "{row}"
            );
        }
    }
}
