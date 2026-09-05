//! Credential availability policies from agent/credential_pool.py.
//! Store hydration, refresh and lease ownership are still being ported.
#![allow(dead_code)]
use serde_json::Value;

const ENTRY_FIELDS: &[&str] = &[
    "id",
    "label",
    "auth_type",
    "priority",
    "source",
    "access_token",
    "refresh_token",
    "last_status",
    "last_status_at",
    "last_error_code",
    "last_error_reason",
    "last_error_message",
    "last_error_reset_at",
    "base_url",
    "expires_at",
    "expires_at_ms",
    "last_refresh",
    "inference_base_url",
    "agent_key",
    "agent_key_expires_at",
    "request_count",
];
const EXTRA_FIELDS: &[&str] = &[
    "token_type",
    "scope",
    "client_id",
    "portal_base_url",
    "obtained_at",
    "expires_in",
    "agent_key_id",
    "agent_key_expires_in",
    "agent_key_reused",
    "agent_key_obtained_at",
    "tls",
    "secret_source",
    "secret_fingerprint",
    "failure_reason",
];
const ALWAYS_EMIT: &[&str] = &[
    "last_status",
    "last_status_at",
    "last_error_code",
    "last_error_reason",
    "last_error_message",
    "last_error_reset_at",
];

/// In-memory entries retain live secrets. Only disk serialization sanitizes
/// borrowed values. Omit Debug to keep raw keys out of diagnostic output.
#[derive(Clone)]
pub struct PooledCredential {
    provider: String,
    fields: serde_json::Map<String, Value>,
    extra: serde_json::Map<String, Value>,
}

impl PooledCredential {
    pub fn from_dict(provider: &str, payload: &Value) -> anyhow::Result<Self> {
        let id = if payload.get("id").is_none() {
            crate::install_identity::mint_id()
                .ok_or_else(|| anyhow::anyhow!("cannot generate credential identity"))?[..6]
                .to_owned()
        } else {
            String::new()
        };
        Self::decode(provider, payload, &id)
    }

    fn decode(provider: &str, payload: &Value, missing_id: &str) -> anyhow::Result<Self> {
        let payload = payload
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("credential entry must be an object"))?;
        let mut fields = serde_json::Map::new();
        for key in ENTRY_FIELDS {
            fields.insert(
                (*key).into(),
                payload.get(*key).cloned().unwrap_or_else(|| match *key {
                    "id" => Value::String(missing_id.into()),
                    "label" => payload
                        .get("source")
                        .cloned()
                        .unwrap_or_else(|| Value::String(provider.into())),
                    "auth_type" => Value::String("api_key".into()),
                    "source" => Value::String("manual".into()),
                    "access_token" => Value::String(String::new()),
                    "priority" | "request_count" => serde_json::json!(0),
                    _ => Value::Null,
                }),
            );
        }
        if fields["last_status_at"].is_string() {
            fields.insert(
                "last_status_at".into(),
                serde_json::json!(absolute_timestamp(&fields["last_status_at"])),
            );
        }
        let auth = if provider == "anthropic"
            && fields["access_token"]
                .as_str()
                .is_some_and(|token| token.starts_with("sk-ant-oat"))
        {
            "oauth".into()
        } else if crate::python_value::truthy(&fields["auth_type"]) {
            fields["auth_type"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| crate::python_value::python_repr(&fields["auth_type"]))
        } else {
            "api_key".into()
        };
        fields.insert("auth_type".into(), Value::String(auth));
        let mut extra = serde_json::Map::new();
        for key in EXTRA_FIELDS {
            if let Some(value) = payload.get(*key).filter(|value| !value.is_null()) {
                extra.insert((*key).into(), value.clone());
            }
        }
        Ok(Self {
            provider: provider.into(),
            fields,
            extra,
        })
    }

    pub fn to_dict(&self) -> Value {
        let mut fields: serde_json::Map<String, Value> = self
            .fields
            .iter()
            .filter(|(key, value)| !value.is_null() || ALWAYS_EMIT.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        fields.extend(
            self.extra
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        Value::Object(crate::credential_persistence::sanitize(
            &fields,
            &self.provider,
        ))
    }

    /// Nous validity belongs to the NAS token validator; other providers use
    /// their access token unchanged, including Python's scalar coercion.
    pub fn runtime_key(&self, mut nous_usable: impl FnMut(&str, &Value, &Value) -> bool) -> String {
        if self.provider == "nous" {
            for (key, expires) in [
                ("agent_key", "agent_key_expires_at"),
                ("access_token", "expires_at"),
            ] {
                if let Some(token) = self.fields[key].as_str().filter(|token| {
                    !token
                        .trim_matches(crate::python_value::python_whitespace)
                        .is_empty()
                }) {
                    if nous_usable(
                        token,
                        self.extra.get("scope").unwrap_or(&Value::Null),
                        &self.fields[expires],
                    ) {
                        return token
                            .trim_matches(crate::python_value::python_whitespace)
                            .into();
                    }
                }
            }
            return String::new();
        }
        let token = &self.fields["access_token"];
        if !crate::python_value::truthy(token) {
            return String::new();
        }
        token
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(token))
    }

    pub fn cooldown_until(&self, sole: bool) -> Option<f64> {
        let mut fields = self.fields.clone();
        if let Some(reason) = self.extra.get("failure_reason") {
            fields.insert("failure_reason".into(), reason.clone());
        }
        exhausted_until(&Value::Object(fields), sole)
    }

    pub fn runtime_base_url(&self) -> &Value {
        if self.provider == "nous"
            && crate::python_value::truthy(&self.fields["inference_base_url"])
        {
            &self.fields["inference_base_url"]
        } else {
            &self.fields["base_url"]
        }
    }

    // ---- read accessors used by the pool selection layer -------------------

    pub fn id(&self) -> &str {
        self.fields["id"].as_str().unwrap_or("")
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// `auth_type`, always a normalized string (`decode` guarantees it).
    pub fn auth_type(&self) -> &str {
        self.fields["auth_type"].as_str().unwrap_or("api_key")
    }

    pub fn source(&self) -> &str {
        self.fields["source"].as_str().unwrap_or("")
    }

    /// `last_status`, or `None` when the field is JSON null (never set).
    pub fn last_status(&self) -> Option<&str> {
        self.fields["last_status"].as_str()
    }

    /// `priority`, coerced like Python `int()` on the stored value.
    pub fn priority(&self) -> i64 {
        self.fields["priority"].as_i64().unwrap_or(0)
    }

    pub fn request_count(&self) -> i64 {
        self.fields["request_count"].as_i64().unwrap_or(0)
    }

    /// Epoch seconds of the last status transition, or `None` when unset/zero
    /// (matching Python `entry.last_status_at or 0` guards).
    pub fn last_status_at(&self) -> Option<f64> {
        self.fields["last_status_at"].as_f64()
    }

    pub fn access_token(&self) -> &str {
        self.fields["access_token"].as_str().unwrap_or("")
    }

    /// Return a copy with the exhaustion status cleared back to `ok`, matching
    /// the `clear_expired` reset in `_available_entries` (status -> ok, all the
    /// `last_error_*`/`last_status_at` fields cleared to null).
    pub fn with_cleared_status(&self) -> Self {
        let mut next = self.clone();
        next.fields
            .insert("last_status".into(), Value::String("ok".into()));
        for key in [
            "last_status_at",
            "last_error_code",
            "last_error_reason",
            "last_error_message",
            "last_error_reset_at",
        ] {
            next.fields.insert(key.into(), Value::Null);
        }
        next
    }

    /// Return a copy with `request_count` set (LEAST_USED distributes load by
    /// bumping the selected entry's counter).
    pub fn with_request_count(&self, count: i64) -> Self {
        let mut next = self.clone();
        next.fields
            .insert("request_count".into(), serde_json::json!(count));
        next
    }

    /// Return a copy with `priority` set (ROUND_ROBIN re-numbers entries).
    pub fn with_priority(&self, priority: i64) -> Self {
        let mut next = self.clone();
        next.fields
            .insert("priority".into(), serde_json::json!(priority));
        next
    }
}

/// Refresh one source in memory. The return value means its disk-safe state
/// changed, not merely that a borrowed runtime token was hydrated.
pub fn upsert_entry(
    entries: &mut Vec<PooledCredential>,
    provider: &str,
    source: &str,
    payload: &mut Value,
) -> anyhow::Result<bool> {
    let mut seen = false;
    let before = entries.len();
    entries.retain(|entry| {
        if entry.fields["source"] != source {
            return true;
        }
        if seen {
            false
        } else {
            seen = true;
            true
        }
    });
    let deduplicated = entries.len() != before;
    let payload = payload
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("credential update must be an object"))?;
    let Some(index) = entries
        .iter()
        .position(|entry| entry.fields["source"] == source)
    else {
        let priority = entries
            .iter()
            .map(|entry| {
                entry.fields["priority"]
                    .as_i64()
                    .ok_or_else(|| anyhow::anyhow!("credential priority must be an integer"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(-1)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("credential priority overflow"))?;
        if !payload.contains_key("id") {
            let id = crate::install_identity::mint_id()
                .ok_or_else(|| anyhow::anyhow!("cannot generate credential identity"))?;
            payload.insert("id".into(), Value::String(id[..6].into()));
        }
        payload
            .entry("priority")
            .or_insert(serde_json::json!(priority));
        payload
            .entry("label")
            .or_insert(Value::String(source.into()));
        entries.push(PooledCredential::from_dict(
            provider,
            &Value::Object(payload.clone()),
        )?);
        return Ok(true);
    };
    let existing = &entries[index];
    let incoming = payload.get("access_token").filter(|value| !value.is_null());
    let mut token_changed = incoming.is_some_and(|token| token != &existing.fields["access_token"]);
    if token_changed && !crate::python_value::truthy(&existing.fields["access_token"]) {
        if let Some(known) = existing
            .extra
            .get("secret_fingerprint")
            .and_then(Value::as_str)
            .filter(|known| !known.is_empty())
        {
            token_changed = incoming
                .and_then(crate::credential_persistence::fingerprint)
                .as_deref()
                != Some(known);
        }
    }
    let mut updated = existing.clone();
    let mut extra_updates = serde_json::Map::new();
    let mut changed = false;
    for (key, value) in payload.iter() {
        if matches!(key.as_str(), "id" | "priority") || value.is_null() {
            continue;
        }
        if key == "label" && crate::python_value::truthy(&existing.fields["label"]) {
            continue;
        }
        if ENTRY_FIELDS.contains(&key.as_str()) {
            if existing.fields[key] != *value {
                updated.fields.insert(key.clone(), value.clone());
                changed = true;
            }
        } else if key == "provider" {
            let provider = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("credential provider must be a string"))?;
            if provider != existing.provider {
                updated.provider = provider.into();
                changed = true;
            }
        } else if key == "extra" {
            let extra = value
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("credential extra must be an object"))?;
            if extra != &existing.extra {
                updated.extra = extra.clone();
                changed = true;
            }
        } else if EXTRA_FIELDS.contains(&key.as_str()) && existing.extra.get(key) != Some(value) {
            extra_updates.insert(key.clone(), value.clone());
        }
    }
    if token_changed && !existing.fields["last_status"].is_null() {
        for key in ALWAYS_EMIT {
            updated.fields.insert((*key).into(), Value::Null);
        }
        changed = true;
    }
    if !extra_updates.is_empty() {
        // dataclasses.replace receives the merged original extra map when flat
        // metadata changes, even when a separate extra payload was supplied.
        updated.extra = existing.extra.clone();
        updated.extra.extend(extra_updates);
        changed = true;
    }
    if !changed {
        return Ok(deduplicated);
    }
    let auth = if updated.provider == "anthropic"
        && updated.fields["access_token"]
            .as_str()
            .is_some_and(|token| token.starts_with("sk-ant-oat"))
    {
        "oauth".into()
    } else if crate::python_value::truthy(&updated.fields["auth_type"]) {
        updated.fields["auth_type"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::python_value::python_repr(&updated.fields["auth_type"]))
    } else {
        "api_key".into()
    };
    updated
        .fields
        .insert("auth_type".into(), Value::String(auth));
    let disk_changed = deduplicated || existing.to_dict() != updated.to_dict();
    entries[index] = updated;
    Ok(disk_changed)
}

fn manual_source(source: &str) -> bool {
    let source = source
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase();
    source == "manual" || source.starts_with("manual:")
}

/// Ordinary pool loads must pass false for prune_env_sources: an environment
/// miss in this process does not prove that another process lost its secret.
pub fn prune_stale_sources(
    entries: &mut Vec<PooledCredential>,
    active: &std::collections::HashSet<String>,
    prune_env_sources: bool,
) -> anyhow::Result<bool> {
    let mut keep = Vec::with_capacity(entries.len());
    for entry in entries.iter() {
        let source = entry.fields["source"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("credential source must be a string"))?;
        let prunable = if source.starts_with("env:") {
            prune_env_sources
        } else {
            crate::credential_persistence::is_borrowed(
                &entry.fields["source"],
                &Value::String(entry.provider.clone()),
            ) || source == "hermes_pkce"
        };
        keep.push(manual_source(source) || active.contains(source) || !prunable);
    }
    let changed = keep.iter().any(|keep| !keep);
    let mut keep = keep.into_iter();
    entries.retain(|_| keep.next().expect("one decision per entry"));
    Ok(changed)
}

/// Anthropic manual credentials lead seeded credentials; source precedence is
/// applied to priority values without rearranging the persisted row order.
pub fn normalize_priorities(
    provider: &str,
    entries: &mut [PooledCredential],
) -> anyhow::Result<bool> {
    if provider != "anthropic" {
        return Ok(false);
    }
    let ranks = [
        "env:ANTHROPIC_TOKEN",
        "env:CLAUDE_CODE_OAUTH_TOKEN",
        "hermes_pkce",
        "claude_code",
        "env:ANTHROPIC_API_KEY",
    ];
    let mut ordered = Vec::with_capacity(entries.len());
    for entry in entries.iter() {
        let source = entry.fields["source"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("credential source must be a string"))?;
        let manual = manual_source(source);
        let priority = entry.fields["priority"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("credential priority must be an integer"))?;
        let label = if manual {
            ""
        } else {
            entry.fields["label"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("credential label must be a string"))?
        };
        let rank = if manual {
            0
        } else {
            ranks
                .iter()
                .position(|rank| *rank == source)
                .unwrap_or(ranks.len())
        };
        ordered.push(((!manual, rank, priority, label.to_owned()), entry.clone()));
    }
    ordered.sort_by(|left, right| left.0.cmp(&right.0));
    let mut changed = false;
    for (priority, (_, mut entry)) in ordered.into_iter().enumerate() {
        if entry.fields["priority"].as_i64() == Some(priority as i64) {
            continue;
        }
        // Python's id-to-index mapping keeps the last index for duplicate IDs.
        let index = entries
            .iter()
            .rposition(|candidate| candidate.fields["id"] == entry.fields["id"])
            .expect("snapshot entry still has an ID");
        entry
            .fields
            .insert("priority".into(), serde_json::json!(priority));
        entries[index] = entry;
        changed = true;
    }
    Ok(changed)
}

/// Decode persisted rows only. Seeding and availability checks must run before
/// this list can be used as a credential pool for requests.
pub fn read_stored_entries(
    profile: &std::path::Path,
    root: Option<&std::path::Path>,
    provider: &str,
) -> anyhow::Result<Vec<PooledCredential>> {
    crate::auth_store::read_pool(profile, root, Some(provider))?
        .as_array()
        .expect("provider slice is an array")
        .iter()
        .map(|payload| PooledCredential::from_dict(provider, payload))
        .collect()
}

/// A sole credential gets a short transient cooldown, but verified billing
/// exhaustion keeps the full cooldown even when there is no alternate key.
pub fn exhausted_ttl(code: Option<i64>, sole: bool, reason: Option<&str>) -> f64 {
    if code == Some(401) {
        return 300.0;
    }
    if reason == Some("billing_unverified") && code != Some(402)
        || sole && code != Some(402) && reason != Some("billing")
    {
        return 60.0;
    }
    3600.0
}

/// Provider reset values accept seconds, milliseconds and ISO timestamps.
/// Python treats nonpositive numeric values differently from numeric strings.
pub fn absolute_timestamp(value: &Value) -> Option<f64> {
    let scale = |n: f64| {
        if n > 1_000_000_000_000.0 {
            n / 1000.0
        } else {
            n
        }
    };
    match value {
        Value::Bool(value) => value.then_some(1.0),
        Value::Number(n) => n.as_f64().filter(|n| *n > 0.0).map(scale),
        Value::String(raw) => {
            let raw = raw.trim_matches(crate::python_value::python_whitespace);
            if let Some(n) =
                crate::python_value::numeric_text(raw).and_then(|raw| raw.parse::<f64>().ok())
            {
                return Some(scale(n));
            }
            let iso = raw.replace('Z', "+00:00");
            crate::message_timestamps::parse_iso_string(&iso, None)
        }
        _ => None,
    }
}

/// Reset deadlines take precedence over the fallback TTL. Missing status time
/// means no deadline, rather than an invented cooldown starting at load time.
/// Like PooledCredential after hydration, last_status_at must be epoch seconds.
pub fn exhausted_until(entry: &Value, sole: bool) -> Option<f64> {
    if entry["last_status"] != "exhausted" {
        return None;
    }
    if let Some(reset) = absolute_timestamp(&entry["last_error_reset_at"]) {
        return Some(reset);
    }
    let status_at = entry["last_status_at"].as_f64().filter(|n| *n != 0.0)?;
    Some(
        status_at
            + exhausted_ttl(
                entry["last_error_code"].as_i64(),
                sole,
                entry["failure_reason"].as_str(),
            ),
    )
}

/// Extract the vendor's retry delay, preserving Python's pattern priority.
pub fn retry_delay(message: &str) -> Option<f64> {
    use std::sync::LazyLock;
    static PATTERNS: LazyLock<Vec<fancy_regex::Regex>> = LazyLock::new(|| {
        [
            r#"quotaResetDelay[:\s\"]+(\d+(?:\.\d+)?)(ms|s)"#,
            r"retry\s+(?:after\s+)?(\d+(?:\.\d+)?)\s*(?:sec|secs|seconds|s\b)",
            r"resets?\s+in\s+(\d+)\s*hr\s+(\d+)\s*min",
            r"resets?\s+in\s+(\d+)\s*hr\b",
            r"resets?\s+in\s+(\d+)\s*min\b",
        ]
        .into_iter()
        .map(|pattern| {
            fancy_regex::Regex::new(&format!("(?i){}", pattern.replace(r"\s", r"[\s\x1c-\x1f]")))
                .unwrap()
        })
        .collect()
    });
    for (index, pattern) in PATTERNS.iter().enumerate() {
        if let Some(caps) = pattern.captures(message).ok().flatten() {
            let number = |i| {
                crate::python_value::numeric_text(caps.get(i)?.as_str())?
                    .parse::<f64>()
                    .ok()
            };
            let value = number(1)?;
            return Some(match index {
                0 if caps[2].eq_ignore_ascii_case("ms") => value / 1000.0,
                0 | 1 => value,
                2 => value * 3600.0 + number(2)? * 60.0,
                3 => value * 3600.0,
                _ => value * 60.0,
            });
        }
    }
    None
}

pub fn normalize_error_context(context: &Value, now: f64) -> Value {
    let Some(context) = context.as_object() else {
        return serde_json::json!({});
    };
    let mut result = serde_json::Map::new();
    for key in ["reason", "message"] {
        if let Some(text) = context
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.trim_matches(crate::python_value::python_whitespace))
            .filter(|text| !text.is_empty())
        {
            result.insert(key.into(), Value::String(text.into()));
        }
    }
    let reset = ["reset_at", "resets_at", "retry_until"]
        .iter()
        .filter_map(|key| context.get(*key))
        .find(|value| crate::python_value::truthy(value));
    let reset = reset.and_then(absolute_timestamp).or_else(|| {
        context
            .get("message")
            .and_then(Value::as_str)
            .and_then(retry_delay)
            .map(|delay| now + delay)
    });
    if let Some(reset) = reset {
        result.insert("reset_at".into(), serde_json::json!(reset));
    }
    Value::Object(result)
}

// ---------------------------------------------------------------------------
// CredentialPool selection core (agent/credential_pool.py CredentialPool)
// ---------------------------------------------------------------------------
//
// This is the availability + current-key selection layer only. Store hydration
// (load_pool + singleton seeding), OAuth token refresh, the provider-specific
// auth-store sync branches (anthropic/nous/openai-codex/xai-oauth) and lease
// ownership are NOT ported here and are documented as deferred. For an API-key
// provider none of those branches execute, so this reproduces the real Python
// selection behavior exactly for that path; a pool whose entries are OAuth /
// device-code sources is out of this slice's scope.

const STATUS_OK: &str = "ok";
const STATUS_EXHAUSTED: &str = "exhausted";
const STATUS_DEAD: &str = "dead";
const AUTH_TYPE_OAUTH: &str = "oauth";
const AUTH_TYPE_API_KEY: &str = "api_key";
const STRATEGY_FILL_FIRST: &str = "fill_first";
const STRATEGY_ROUND_ROBIN: &str = "round_robin";
const STRATEGY_RANDOM: &str = "random";
const STRATEGY_LEAST_USED: &str = "least_used";
const SUPPORTED_POOL_STRATEGIES: &[&str] = &[
    STRATEGY_FILL_FIRST,
    STRATEGY_ROUND_ROBIN,
    STRATEGY_RANDOM,
    STRATEGY_LEAST_USED,
];
const SOURCE_MANUAL: &str = "manual";
/// 24h quiet window before a DEAD manual entry is pruned.
const DEAD_MANUAL_PRUNE_TTL_SECONDS: f64 = 24.0 * 60.0 * 60.0;

/// Port of `_is_manual_source`: the source is `manual` or `manual:<...>`.
fn is_manual_source(source: &str) -> bool {
    let normalized = source
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase();
    normalized == SOURCE_MANUAL || normalized.starts_with(&format!("{SOURCE_MANUAL}:"))
}

/// Port of `get_pool_strategy`: read `credential_pool_strategies[provider]` from
/// the resolved config, defaulting to `fill_first` for a missing/unknown value.
/// The caller supplies the loaded config so the pool stays free of config I/O.
pub fn pool_strategy(provider: &str, config: &Value) -> String {
    let strategies = match config.get("credential_pool_strategies") {
        Some(Value::Object(map)) => map,
        _ => return STRATEGY_FILL_FIRST.to_string(),
    };
    let raw = strategies.get(provider).cloned().unwrap_or(Value::Null);
    let strategy = if crate::python_value::truthy(&raw) {
        raw.as_str().unwrap_or("").to_string()
    } else {
        String::new()
    };
    let strategy = strategy
        .trim_matches(crate::python_value::python_whitespace)
        .to_lowercase();
    if SUPPORTED_POOL_STRATEGIES.contains(&strategy.as_str()) {
        strategy
    } else {
        STRATEGY_FILL_FIRST.to_string()
    }
}

/// Sink for persisting the pool after a mutation (clear-expired, prune,
/// round-robin renumber). Mirrors Python `persist_pool_entries(provider,
/// [to_dict...], removed_ids)`; the pool owns WHEN to persist, the sink owns HOW.
pub type PersistSink = Box<dyn FnMut(&str, Vec<Value>, Vec<String>) + Send>;

/// The runtime credential pool for one provider. Entries are kept sorted by
/// ascending priority. `select`/`peek`/availability run over a shared clock so
/// tests are deterministic.
pub struct CredentialPool {
    provider: String,
    entries: Vec<PooledCredential>,
    current_id: Option<String>,
    strategy: String,
    clock: Box<dyn Fn() -> f64 + Send>,
    persist: Option<PersistSink>,
    /// STRATEGY_RANDOM index chooser over `[0, len)`. Injectable for tests;
    /// defaults to a kernel-seeded uniform pick.
    choose_random: Box<dyn FnMut(usize) -> usize + Send>,
}

fn default_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl CredentialPool {
    /// Construct a pool. `entries` are sorted by ascending priority (stable),
    /// matching Python `sorted(entries, key=priority)`.
    pub fn new(provider: &str, mut entries: Vec<PooledCredential>, strategy: &str) -> Self {
        entries.sort_by_key(|e| e.priority());
        Self {
            provider: provider.to_string(),
            entries,
            current_id: None,
            strategy: strategy.to_string(),
            clock: Box::new(default_now),
            persist: None,
            choose_random: Box::new(|len| {
                if len <= 1 {
                    0
                } else {
                    // Non-security load spread; a weak uniform pick is fine.
                    (default_now().to_bits() as usize) % len
                }
            }),
        }
    }

    /// Inject a fixed clock (tests / reproducible selection windows).
    pub fn with_clock(mut self, clock: impl Fn() -> f64 + Send + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Install the persistence sink invoked after a mutating availability pass.
    pub fn with_persist(mut self, persist: PersistSink) -> Self {
        self.persist = Some(persist);
        self
    }

    /// Inject the STRATEGY_RANDOM chooser (tests pin it deterministically).
    pub fn with_random_chooser(
        mut self,
        chooser: impl FnMut(usize) -> usize + Send + 'static,
    ) -> Self {
        self.choose_random = Box::new(chooser);
        self
    }

    pub fn has_credentials(&self) -> bool {
        !self.entries.is_empty()
    }

    pub fn has_available(&mut self) -> bool {
        !self.available_entries(false).is_empty()
    }

    pub fn entries(&self) -> &[PooledCredential] {
        &self.entries
    }

    /// The runtime key of an API-key entry. Non-nous providers return the
    /// access token unchanged; nous validation is out of this slice, so a nous
    /// key resolves via the existing `runtime_key` with a never-usable stub.
    fn runtime_api_key(entry: &PooledCredential) -> String {
        entry.runtime_key(|_, _, _| false)
    }

    /// Earliest epoch any entry re-enters rotation, or `None` when one is
    /// available now or no exhausted entry has a usable recovery time.
    pub fn next_available_at(&mut self) -> Option<f64> {
        if !self.available_entries(false).is_empty() {
            return None;
        }
        let sole = self.sole_credential();
        let mut candidates: Vec<f64> = Vec::new();
        for entry in &self.entries {
            if entry.last_status() != Some(STATUS_EXHAUSTED) {
                continue;
            }
            if let Some(until) = entry.cooldown_until(sole) {
                candidates.push(until);
            }
        }
        candidates.into_iter().reduce(f64::min)
    }

    fn current_unlocked(&self) -> Option<&PooledCredential> {
        let id = self.current_id.as_deref()?;
        self.entries.iter().find(|e| e.id() == id)
    }

    pub fn current(&self) -> Option<&PooledCredential> {
        self.current_unlocked()
    }

    /// Stable id for the runtime credential in use. Prefer the current
    /// selection when it still supplies `api_key_hint`; otherwise fall back to
    /// an unambiguous key match. `None` hint returns the current id (if any).
    pub fn entry_id_for_api_key(&self, api_key_hint: Option<&str>) -> Option<String> {
        if let Some(current) = self.current_unlocked() {
            if api_key_hint.is_none()
                || Some(Self::runtime_api_key(current).as_str()) == api_key_hint
            {
                return Some(current.id().to_string());
            }
        }
        let hint = api_key_hint?;
        let matches: Vec<&PooledCredential> = self
            .entries
            .iter()
            .filter(|e| Self::runtime_api_key(e) == hint)
            .collect();
        if matches.len() == 1 {
            Some(matches[0].id().to_string())
        } else {
            None
        }
    }

    /// DEAD entries never re-enter rotation, so a single non-DEAD entry means
    /// there is nothing to rotate to (the sole-credential short cooldown).
    fn sole_credential(&self) -> bool {
        self.entries
            .iter()
            .filter(|e| e.last_status() != Some(STATUS_DEAD))
            .count()
            <= 1
    }

    /// Port of `_available_entries` for the API-key path. Returns the ids of
    /// entries not in cooldown, in priority order. When `clear_expired` is set,
    /// entries whose cooldown elapsed are reset to `ok` and the pool persists;
    /// aged-out DEAD manual entries are pruned and the pool persists.
    ///
    /// Deferred vs Python (never reached for API-key entries, documented so a
    /// future OAuth port knows the seam): the anthropic/nous/codex/xai
    /// auth-store sync branches, the single-use-token refresh defer, the codex
    /// early-reopen probe, and the OAuth empty-access-token guard.
    fn available_entries(&mut self, clear_expired: bool) -> Vec<String> {
        let now = (self.clock)();
        let sole = self.sole_credential();
        let mut cleared_any = false;
        let mut prune: Vec<String> = Vec::new();
        let mut available: Vec<String> = Vec::new();
        // Index-based so we can replace an entry in place on clear.
        for idx in 0..self.entries.len() {
            let entry = &self.entries[idx];
            if entry.auth_type() == AUTH_TYPE_API_KEY && Self::runtime_api_key(entry).is_empty() {
                continue;
            }
            match entry.last_status() {
                Some(STATUS_DEAD) => {
                    if is_manual_source(entry.source()) {
                        let dead_at = entry.last_status_at().unwrap_or(0.0);
                        if dead_at != 0.0 && now - dead_at > DEAD_MANUAL_PRUNE_TTL_SECONDS {
                            prune.push(entry.id().to_string());
                            cleared_any = true;
                        }
                    }
                    continue;
                }
                Some(STATUS_EXHAUSTED) => {
                    let until = entry.cooldown_until(sole);
                    if let Some(until) = until {
                        if now < until {
                            continue;
                        }
                    }
                    if clear_expired {
                        let cleared = entry.with_cleared_status();
                        self.entries[idx] = cleared;
                        cleared_any = true;
                    }
                }
                _ => {}
            }
            // API-key entries never need refresh; OAuth refresh is deferred.
            available.push(self.entries[idx].id().to_string());
        }
        if !prune.is_empty() {
            let pruned: std::collections::HashSet<&str> =
                prune.iter().map(String::as_str).collect();
            self.entries.retain(|e| !pruned.contains(e.id()));
        }
        if cleared_any {
            self.persist_now(prune.clone());
        }
        // `available` holds ids captured before pruning; drop any pruned ids so
        // the returned set matches the surviving entries.
        let pruned: std::collections::HashSet<&str> = prune.iter().map(String::as_str).collect();
        available.retain(|id| !pruned.contains(id.as_str()));
        available
    }

    fn persist_now(&mut self, removed_ids: Vec<String>) {
        let provider = self.provider.clone();
        let snapshot: Vec<Value> = self.entries.iter().map(|e| e.to_dict()).collect();
        if let Some(persist) = self.persist.as_mut() {
            persist(&provider, snapshot, removed_ids);
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id() == id)
    }

    /// Port of `_select_unlocked`. Selects the best available entry by the
    /// configured strategy, updates the current cursor, and applies the
    /// strategy's side effects (least-used counter bump, round-robin renumber
    /// + persist).
    fn select_unlocked(&mut self) -> Option<String> {
        let available = self.available_entries(true);
        if available.is_empty() {
            self.current_id = None;
            tracing::info!("credential pool: no available entries (all exhausted or empty)");
            return None;
        }

        if self.strategy == STRATEGY_RANDOM {
            let pick = (self.choose_random)(available.len()).min(available.len() - 1);
            let id = available[pick].clone();
            self.current_id = Some(id.clone());
            return Some(id);
        }

        if self.strategy == STRATEGY_LEAST_USED && available.len() > 1 {
            // min by request_count, ties broken by first (priority order).
            let id = available
                .iter()
                .min_by_key(|id| {
                    self.index_of(id)
                        .map(|i| self.entries[i].request_count())
                        .unwrap_or(i64::MAX)
                })
                .unwrap()
                .clone();
            if let Some(i) = self.index_of(&id) {
                let bumped =
                    self.entries[i].with_request_count(self.entries[i].request_count() + 1);
                self.entries[i] = bumped;
            }
            self.current_id = Some(id.clone());
            return Some(id);
        }

        if self.strategy == STRATEGY_ROUND_ROBIN && available.len() > 1 {
            let id = available[0].clone();
            // Move the chosen entry to the back, then renumber priorities 0..n
            // to match `[replace(c, priority=idx) for idx, c in enumerate(...)]`.
            let chosen_pos = self.index_of(&id).unwrap();
            let chosen = self.entries.remove(chosen_pos);
            self.entries.push(chosen);
            let renumbered: Vec<PooledCredential> = self
                .entries
                .iter()
                .enumerate()
                .map(|(idx, e)| e.with_priority(idx as i64))
                .collect();
            self.entries = renumbered;
            self.persist_now(Vec::new());
            self.current_id = Some(id.clone());
            return Some(id);
        }

        // fill_first / default: highest-precedence available entry.
        let id = available[0].clone();
        self.current_id = Some(id.clone());
        Some(id)
    }

    /// Port of `select`. Returns the selected entry's id, or `None` when the
    /// pool is empty or fully exhausted. (Single-use-token refresh, which
    /// Python runs outside the lock and then re-selects, is deferred; it never
    /// applies to API-key providers.)
    pub fn select(&mut self) -> Option<String> {
        self.select_unlocked()
    }

    /// Port of `peek`: the current selection if any, else the first available
    /// entry by priority (strategy is NOT applied to a peek).
    pub fn peek(&mut self) -> Option<String> {
        if let Some(current) = self.current_unlocked() {
            return Some(current.id().to_string());
        }
        self.available_entries(false).into_iter().next()
    }

    /// The runtime key of the peeked entry, or `None` when the stripped key is
    /// empty. Mirrors the STT/TTS resolver in tools/tool_backend_helpers.py:
    /// `str(entry.runtime_api_key or entry.access_token or "").strip()`. This is
    /// the exact shape tool_credentials::provider_secret's pool callback wants.
    pub fn peek_runtime_key(&mut self) -> Option<String> {
        let id = self.peek()?;
        let entry = self.entries.iter().find(|e| e.id() == id)?;
        let mut key = Self::runtime_api_key(entry);
        if key.is_empty() {
            key = entry.access_token().to_string();
        }
        let key = key
            .trim_matches(crate::python_value::python_whitespace)
            .to_string();
        if key.is_empty() {
            None
        } else {
            Some(key)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn pool_selection_matches_python() {
        let data: Value = serde_json::from_str(include_str!(
            "../../../tools/credential-pool-select-goldens.json"
        ))
        .unwrap();
        let now = data["now"].as_f64().unwrap();
        for row in data["rows"].as_array().unwrap() {
            let name = row["name"].as_str().unwrap();
            let provider = row["provider"].as_str().unwrap();
            let strategy = row["strategy"].as_str().unwrap();
            let entries: Vec<PooledCredential> = row["entries_in"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| PooledCredential::from_dict(provider, d).unwrap())
                .collect();

            // Capture persist calls as (count, removed_ids), matching the oracle.
            type PersistLog = Arc<Mutex<Vec<(usize, Vec<String>)>>>;
            let log: PersistLog = Arc::new(Mutex::new(Vec::new()));
            let sink_log = log.clone();
            let mut pool = CredentialPool::new(provider, entries, strategy)
                .with_clock(move || now)
                .with_persist(Box::new(move |_provider, snapshot, removed| {
                    sink_log.lock().unwrap().push((snapshot.len(), removed));
                }));

            assert_eq!(
                pool.has_credentials(),
                row["has_credentials"].as_bool().unwrap(),
                "{name}: has_credentials"
            );
            assert_eq!(
                pool.has_available(),
                row["has_available"].as_bool().unwrap(),
                "{name}: has_available"
            );
            let next = pool.next_available_at();
            let exp_next = row["next_available_at"].as_f64();
            assert_eq!(next, exp_next, "{name}: next_available_at");

            assert_eq!(pool.peek().as_deref(), row["peek"].as_str(), "{name}: peek");
            assert_eq!(
                pool.select().as_deref(),
                row["select_1"].as_str(),
                "{name}: select_1"
            );
            assert_eq!(
                pool.current().map(|e| e.id()),
                row["current_after_1"].as_str(),
                "{name}: current_after_1"
            );
            assert_eq!(
                pool.select().as_deref(),
                row["select_2"].as_str(),
                "{name}: select_2"
            );

            // entries_after: id, priority, last_status, request_count, access_token.
            let got: Vec<(String, i64, Option<String>, i64, String)> = pool
                .entries()
                .iter()
                .map(|e| {
                    (
                        e.id().to_string(),
                        e.priority(),
                        e.last_status().map(str::to_string),
                        e.request_count(),
                        e.access_token().to_string(),
                    )
                })
                .collect();
            let want: Vec<(String, i64, Option<String>, i64, String)> = row["entries_after"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    (
                        e["id"].as_str().unwrap().to_string(),
                        e["priority"].as_i64().unwrap(),
                        e["last_status"].as_str().map(str::to_string),
                        e["request_count"].as_i64().unwrap(),
                        e["access_token"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            assert_eq!(got, want, "{name}: entries_after");

            // persisted call sequence (count + removed_ids per call).
            let got_persist: Vec<(usize, Vec<String>)> = log.lock().unwrap().clone();
            let want_persist: Vec<(usize, Vec<String>)> = row["persisted"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    (
                        p["count"].as_u64().unwrap() as usize,
                        p["removed_ids"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|v| v.as_str().unwrap().to_string())
                            .collect(),
                    )
                })
                .collect();
            assert_eq!(got_persist, want_persist, "{name}: persisted");

            // entry_id lookups run on the final selected state.
            // Mirror the oracle's `if first_key` guard: a falsy (empty/absent)
            // access token yields None without calling entry_id_for_api_key.
            let first_key = row["entries_in"]
                .as_array()
                .unwrap()
                .first()
                .and_then(|d| d["access_token"].as_str())
                .filter(|k| !k.is_empty());
            let got_first = first_key.and_then(|k| pool.entry_id_for_api_key(Some(k)));
            assert_eq!(
                got_first.as_deref(),
                row["id_for_first_key"].as_str(),
                "{name}: id_for_first_key"
            );
            let got_unknown = pool.entry_id_for_api_key(Some("no-such-key"));
            assert_eq!(
                got_unknown.as_deref(),
                row["id_for_unknown_key"].as_str(),
                "{name}: id_for_unknown_key"
            );
        }
    }

    #[test]
    fn pruning_and_source_priorities_match_python() {
        let rows: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/credential-maintenance-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let provider = row["provider"].as_str().unwrap();
            let mut entries: Vec<_> = row["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| super::PooledCredential::from_dict(provider, entry).unwrap())
                .collect();
            let changed = if row["kind"] == "prune" {
                let active = row["active"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect();
                super::prune_stale_sources(
                    &mut entries,
                    &active,
                    row["prune_env"].as_bool().unwrap(),
                )
                .unwrap()
            } else {
                super::normalize_priorities(provider, &mut entries).unwrap()
            };
            assert_eq!(changed, row["changed"].as_bool().unwrap(), "{row}");
            assert_eq!(
                serde_json::json!(entries
                    .iter()
                    .map(super::PooledCredential::to_dict)
                    .collect::<Vec<_>>()),
                row["result"],
                "{row}"
            );
        }
    }
    #[test]
    fn source_rehydration_and_rotation_match_python() {
        let rows: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tools/credential-upsert-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let provider = row["provider"].as_str().unwrap();
            let mut entries = row["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| super::PooledCredential::from_dict(provider, entry).unwrap())
                .collect();
            let mut payload = row["payload"].clone();
            let changed = super::upsert_entry(
                &mut entries,
                provider,
                row["source"].as_str().unwrap(),
                &mut payload,
            )
            .unwrap();
            assert_eq!(changed, row["changed"].as_bool().unwrap(), "{row}");
            if let Some(expected) = row.get("updated_payload") {
                assert_eq!(&payload, expected, "{row}");
            }
            assert_eq!(
                serde_json::json!(entries
                    .iter()
                    .map(super::PooledCredential::to_dict)
                    .collect::<Vec<_>>()),
                row["result"],
                "{row}"
            );
            assert_eq!(
                serde_json::json!(entries
                    .iter()
                    .map(|entry| entry.runtime_key(|_, _, _| false))
                    .collect::<Vec<_>>()),
                row["runtime"],
                "{row}"
            );
        }
    }
    #[test]
    fn stored_entries_keep_runtime_secrets_and_serialize_borrowed_references() {
        let path = std::env::temp_dir().join(format!(
            "hermes-pool-entries-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        let stored = serde_json::json!({"credential_pool":{"openai-api":[
            {"id":"owned", "source":"manual", "access_token":"owned-fixture"},
            {"id":"borrowed", "source":"env:OPENAI_API_KEY", "access_token":"borrowed-fixture", "last_status":"exhausted", "last_status_at":"2026-09-06T12:34:56Z", "last_error_code":429}
        ]}});
        let bytes = serde_json::to_vec(&stored).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let entries = super::read_stored_entries(&path, None, "openai-api").unwrap();
        assert_eq!(
            entries[0].runtime_key(|_, _, _| panic!("OpenAI key reached Nous validator")),
            "owned-fixture"
        );
        assert_eq!(entries[0].to_dict()["access_token"], "owned-fixture");
        assert_eq!(entries[1].runtime_key(|_, _, _| false), "borrowed-fixture");
        let serialized = entries[1].to_dict();
        assert!(serialized.get("access_token").is_none());
        assert!(serialized["secret_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        assert_eq!(serialized["id"], "borrowed");
        let at = super::absolute_timestamp(&serde_json::json!("2026-09-06T12:34:56Z")).unwrap();
        assert_eq!(entries[1].cooldown_until(true), Some(at + 60.0));
        assert_eq!(entries[1].cooldown_until(false), Some(at + 3600.0));
        let reloaded = super::PooledCredential::from_dict("openai-api", &serialized).unwrap();
        assert_eq!(reloaded.runtime_key(|_, _, _| false), "");
        assert_eq!(reloaded.to_dict(), serialized);
        let mut hydrated = vec![reloaded];
        let mut payload =
            serde_json::json!({"source":"env:OPENAI_API_KEY", "access_token":"borrowed-fixture"});
        assert!(!super::upsert_entry(
            &mut hydrated,
            "openai-api",
            "env:OPENAI_API_KEY",
            &mut payload
        )
        .unwrap());
        assert_eq!(hydrated[0].runtime_key(|_, _, _| false), "borrowed-fixture");
        assert_eq!(hydrated[0].cooldown_until(true), Some(at + 60.0));
        assert_eq!(hydrated[0].to_dict(), serialized);
        payload["access_token"] = serde_json::json!("rotated-fixture");
        assert!(super::upsert_entry(
            &mut hydrated,
            "openai-api",
            "env:OPENAI_API_KEY",
            &mut payload
        )
        .unwrap());
        assert_eq!(hydrated[0].cooldown_until(true), None);
        assert_eq!(hydrated[0].to_dict()["id"], "borrowed");
        assert_ne!(
            hydrated[0].to_dict()["secret_fingerprint"],
            serialized["secret_fingerprint"]
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        hydrated.push(entries[0].clone());
        assert!(!super::prune_stale_sources(&mut hydrated, &Default::default(), false).unwrap());
        assert_eq!(hydrated.len(), 2);
        assert!(super::prune_stale_sources(&mut hydrated, &Default::default(), true).unwrap());
        assert_eq!(hydrated.len(), 1);
        assert_eq!(hydrated[0].runtime_key(|_, _, _| false), "owned-fixture");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn credential_entry_decoding_and_serialization_match_python() {
        let rows: serde_json::Value =
            serde_json::from_str(include_str!("../../../tools/credential-entry-goldens.json"))
                .unwrap();
        for row in rows.as_array().unwrap() {
            let entry = super::PooledCredential::decode(
                row["provider"].as_str().unwrap(),
                &row["payload"],
                "a1b2c3",
            )
            .unwrap();
            assert_eq!(entry.to_dict(), row["result"], "{row}");
            assert_eq!(entry.runtime_base_url(), &row["runtime_base_url"], "{row}");
            assert_eq!(
                entry.runtime_key(|token, _, _| token.trim().starts_with("valid")),
                row["runtime_key"].as_str().unwrap(),
                "{row}"
            );
        }
    }
    #[test]
    fn cooldown_policies_match_python() {
        let rows: Value = serde_json::from_str(include_str!(
            "../../../tools/credential-cooldown-goldens.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let result = match row["kind"].as_str().unwrap() {
                "ttl" => serde_json::json!(exhausted_ttl(
                    row["code"].as_i64(),
                    row["sole"].as_bool().unwrap(),
                    row["reason"].as_str()
                )),
                "until" => serde_json::json!(exhausted_until(
                    &row["entry"],
                    row["sole"].as_bool().unwrap()
                )),
                "timestamp" => serde_json::json!(absolute_timestamp(&row["value"])),
                "delay" => serde_json::json!(retry_delay(row["message"].as_str().unwrap())),
                "context" => normalize_error_context(&row["context"], 1_700_000_000.0),
                _ => unreachable!(),
            };
            assert_eq!(result, row["result"], "{row}");
        }
    }
}
