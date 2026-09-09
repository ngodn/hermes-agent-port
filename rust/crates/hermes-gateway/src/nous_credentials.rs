//! Native Nous Portal inference credential resolution.
//!
//! Nous refresh tokens are single-use. A refresh therefore holds the exact
//! auth-store source lease from re-read through POST and durable write-back.
//! Named profiles that borrow root state write the rotated pair back to root.

use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PROVIDER: &str = "nous";
const DEVICE_CODE_SOURCE: &str = "device_code";
const DEFAULT_PORTAL_URL: &str = "https://portal.nousresearch.com";
pub(crate) const DEFAULT_INFERENCE_URL: &str = "https://inference-api.nousresearch.com/v1";
pub(crate) const DEFAULT_MODEL: &str = "google/gemini-3.6-flash";
const DEFAULT_CLIENT_ID: &str = "hermes-cli";
const INVOKE_SCOPE: &str = "inference:invoke";
const REFRESH_SKEW_SECONDS: f64 = 120.0;
const AUTH_LOCK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub(crate) struct Locator {
    profile_auth: PathBuf,
    root_auth: Option<PathBuf>,
    shared_auth: PathBuf,
    portal_override: Option<String>,
    inference_override: Option<String>,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureKind {
    Unavailable,
    Transient,
    Terminal,
    Persistence,
}

#[derive(Debug)]
pub(crate) struct Failure {
    kind: FailureKind,
    message: String,
}

impl Failure {
    fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn kind(&self) -> FailureKind {
        self.kind
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

impl Locator {
    pub(crate) fn new(
        profile_auth: impl Into<PathBuf>,
        root_auth: Option<PathBuf>,
        shared_auth: impl Into<PathBuf>,
        portal_override: Option<String>,
        inference_override: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            profile_auth: profile_auth.into(),
            root_auth,
            shared_auth: shared_auth.into(),
            portal_override: clean_url(portal_override.as_deref()),
            inference_override: clean_url(inference_override.as_deref()),
            timeout,
        }
    }

    /// A read-only startup gate. Network I/O and refresh happen lazily at the
    /// first request so conversation construction never blocks on OAuth.
    pub(crate) fn is_configured(&self) -> bool {
        if crate::auth_store::source_suppressed(&self.profile_auth, PROVIDER, DEVICE_CODE_SOURCE) {
            return false;
        }
        [
            &self.profile_auth,
            self.root_auth.as_ref().unwrap_or(&self.profile_auth),
        ]
        .into_iter()
        .any(|path| {
            crate::auth_store::load(path)
                .ok()
                .and_then(|store| store["providers"][PROVIDER].as_object().cloned())
                .is_some_and(|state| has_auth_material(&state))
        })
    }

    pub(crate) async fn resolve(
        &self,
        force_refresh: bool,
        stale_access_token: Option<&str>,
    ) -> Result<crate::credential_pool::RuntimeCredential, Failure> {
        let locator = self.clone();
        let stale_access_token = stale_access_token.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            locator.resolve_sync(force_refresh, stale_access_token.as_deref())
        })
        .await
        .map_err(|error| {
            Failure::new(
                FailureKind::Transient,
                format!("Nous credential worker failed: {error}"),
            )
        })?
    }

    fn resolve_sync(
        &self,
        mut force_refresh: bool,
        stale_access_token: Option<&str>,
    ) -> Result<crate::credential_pool::RuntimeCredential, Failure> {
        let mut transaction = crate::auth_store::lock_provider_state(
            &self.profile_auth,
            self.root_auth.as_deref(),
            PROVIDER,
            self.lock_timeout(),
        )
        .map_err(|error| {
            Failure::new(
                FailureKind::Transient,
                format!("Nous auth-store lock failed: {error}"),
            )
        })?
        .ok_or_else(|| {
            Failure::new(
                FailureKind::Unavailable,
                "Nous Portal authentication is not configured",
            )
        })?;
        let mut state = transaction.store()["providers"][PROVIDER]
            .as_object()
            .cloned()
            .ok_or_else(|| {
                Failure::new(
                    FailureKind::Unavailable,
                    "Nous Portal authentication is not configured",
                )
            })?;

        let (mut portal_url, mut stored_inference_url, mut effective_inference_url, mut client_id) =
            self.routing(&state);
        let mut routing_changed =
            install_routing_metadata(&mut state, &portal_url, &stored_inference_url, &client_id);
        let now = now_epoch();
        let mut selected = select_usable_token(&state, now);
        if force_refresh
            && selected
                .as_ref()
                .is_some_and(|token| stale_access_token.is_some_and(|stale| token != stale))
        {
            force_refresh = false;
        }

        if !force_refresh {
            if let Some(token) = selected.take() {
                let changed = install_runtime_aliases(&mut state, &token);
                if routing_changed || changed {
                    persist_state(&mut transaction, &state)?;
                }
                return Ok(runtime_credential(
                    transaction.store(),
                    token,
                    effective_inference_url,
                ));
            }
        }

        // The shared store coordinates refresh-token rotation across profiles.
        // Keep its global lock off the common valid-token path, but hold it
        // from the final peer re-read through POST and durable write-back.
        let _shared_lock =
            crate::auth_store::AuthFileLock::acquire_for(&self.shared_auth, self.lock_timeout())
                .map_err(|error| {
                    Failure::new(
                        FailureKind::Transient,
                        format!("Nous shared-auth lock failed: {error}"),
                    )
                })?;
        let shared_changed = read_json(&self.shared_auth)
            .ok()
            .flatten()
            .is_some_and(|shared| merge_shared_state(&mut state, &shared));
        (
            portal_url,
            stored_inference_url,
            effective_inference_url,
            client_id,
        ) = self.routing(&state);
        routing_changed |=
            install_routing_metadata(&mut state, &portal_url, &stored_inference_url, &client_id);
        let peer_access = state
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .filter(|token| invoke_jwt_status(token, &state, now).is_none())
            .filter(|token| stale_access_token.is_some_and(|stale| *token != stale))
            .map(str::to_owned);
        selected = if force_refresh {
            peer_access.or_else(|| select_usable_token(&state, now))
        } else {
            select_usable_token(&state, now)
        };
        if force_refresh
            && selected
                .as_ref()
                .is_some_and(|token| stale_access_token.is_some_and(|stale| token != stale))
        {
            force_refresh = false;
        }
        if !force_refresh {
            if let Some(token) = selected {
                let changed = install_runtime_aliases(&mut state, &token);
                if shared_changed || routing_changed || changed {
                    persist_state(&mut transaction, &state)?;
                    let _ = write_shared(&self.shared_auth, &state);
                }
                return Ok(runtime_credential(
                    transaction.store(),
                    token,
                    effective_inference_url,
                ));
            }
        }

        let refresh_token = state
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                Failure::new(
                    FailureKind::Unavailable,
                    "Nous Portal has no refresh token; re-authentication is required",
                )
            })?
            .to_owned();
        let client = refresh_client(&state, self.timeout)?;
        let response = client
            .post(format!("{portal_url}/api/oauth/token"))
            .header("Accept", "application/json")
            .header("x-nous-refresh-token", &refresh_token)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id.as_str()),
            ])
            .send()
            .map_err(|error| {
                Failure::new(
                    FailureKind::Transient,
                    format!("Nous Portal refresh request failed: {error}"),
                )
            })?;
        let status = response.status();
        let payload = response.json::<Value>().map_err(|error| {
            Failure::new(
                FailureKind::Transient,
                format!("Nous Portal refresh response was not JSON: {error}"),
            )
        })?;

        if !status.is_success() {
            let code = payload["error"].as_str().unwrap_or("invalid_grant");
            let terminal = matches!(
                code,
                "invalid_grant" | "invalid_token" | "refresh_token_reused"
            );
            if terminal {
                quarantine(&mut state, code);
                persist_state(&mut transaction, &state)?;
                let _ = std::fs::remove_file(&self.shared_auth);
            }
            return Err(Failure::new(
                if terminal {
                    FailureKind::Terminal
                } else {
                    FailureKind::Transient
                },
                format!("Nous Portal refresh failed ({code}, HTTP {status})"),
            ));
        }

        let access_token = payload["access_token"]
            .as_str()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        let Some(access_token) = access_token else {
            quarantine(&mut state, "invalid_token");
            persist_state(&mut transaction, &state)?;
            let _ = std::fs::remove_file(&self.shared_auth);
            return Err(Failure::new(
                FailureKind::Terminal,
                "Nous Portal refresh response omitted access_token",
            ));
        };
        let refresh_token = payload["refresh_token"]
            .as_str()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .unwrap_or(&refresh_token)
            .to_owned();
        let ttl = crate::python_value::integer(&payload["expires_in"])
            .and_then(|value| value.as_i64())
            .unwrap_or(0)
            .max(0);
        let now = chrono::Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl);
        state.insert("access_token".into(), Value::String(access_token.clone()));
        state.insert("refresh_token".into(), Value::String(refresh_token));
        state.insert(
            "token_type".into(),
            payload
                .get("token_type")
                .filter(|value| crate::python_value::truthy(value))
                .cloned()
                .or_else(|| state.get("token_type").cloned())
                .unwrap_or_else(|| json!("Bearer")),
        );
        state.insert(
            "scope".into(),
            payload
                .get("scope")
                .filter(|value| crate::python_value::truthy(value))
                .cloned()
                .or_else(|| state.get("scope").cloned())
                .unwrap_or_else(|| json!(INVOKE_SCOPE)),
        );
        state.insert("obtained_at".into(), Value::String(now.to_rfc3339()));
        state.insert("expires_in".into(), json!(ttl));
        state.insert("expires_at".into(), Value::String(expires_at.to_rfc3339()));
        state.insert("portal_base_url".into(), Value::String(portal_url));
        state.insert(
            "inference_base_url".into(),
            Value::String(
                validate_network_inference_url(payload["inference_base_url"].as_str())
                    .unwrap_or_else(|| DEFAULT_INFERENCE_URL.to_owned()),
            ),
        );
        state.insert("client_id".into(), Value::String(client_id));
        install_runtime_aliases(&mut state, &access_token);

        // Commit the rotated refresh token before validating or using the new
        // access token. A failed post-refresh validation must never replay the
        // consumed predecessor on the next process.
        persist_state(&mut transaction, &state)?;
        let _ = write_shared(&self.shared_auth, &state);
        if invoke_jwt_status(&access_token, &state, now_epoch()).is_some() {
            return Err(Failure::new(
                FailureKind::Transient,
                "Nous Portal returned an access token that is not a usable inference JWT",
            ));
        }
        let effective = self.inference_override.clone().unwrap_or_else(|| {
            state["inference_base_url"]
                .as_str()
                .unwrap_or(&stored_inference_url)
                .to_owned()
        });
        Ok(runtime_credential(
            transaction.store(),
            access_token,
            effective,
        ))
    }

    fn routing(&self, state: &Map<String, Value>) -> (String, String, String, String) {
        let portal = self.portal_override.clone().unwrap_or_else(|| {
            validate_stored_portal_url(state.get("portal_base_url").and_then(Value::as_str))
                .unwrap_or_else(|| DEFAULT_PORTAL_URL.to_owned())
        });
        let stored_inference =
            validate_network_inference_url(state.get("inference_base_url").and_then(Value::as_str))
                .unwrap_or_else(|| DEFAULT_INFERENCE_URL.to_owned());
        let effective_inference = self
            .inference_override
            .clone()
            .unwrap_or_else(|| stored_inference.clone());
        let client_id = state
            .get("client_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_CLIENT_ID)
            .to_owned();
        (portal, stored_inference, effective_inference, client_id)
    }

    fn lock_timeout(&self) -> Duration {
        self.timeout
            .saturating_add(Duration::from_secs(5))
            .max(AUTH_LOCK_TIMEOUT)
    }
}

fn refresh_client(
    state: &Map<String, Value>,
    timeout: Duration,
) -> Result<reqwest::blocking::Client, Failure> {
    let tls = state.get("tls").and_then(Value::as_object);
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    if tls
        .and_then(|tls| tls.get("insecure"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if let Some(path) = tls
        .and_then(|tls| tls.get("ca_bundle"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        let bytes = std::fs::read(path).map_err(|error| {
            Failure::new(
                FailureKind::Transient,
                format!("Nous CA bundle could not be read: {error}"),
            )
        })?;
        let certificates = reqwest::Certificate::from_pem_bundle(&bytes).map_err(|error| {
            Failure::new(
                FailureKind::Transient,
                format!("Nous CA bundle is invalid: {error}"),
            )
        })?;
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    builder.build().map_err(|error| {
        Failure::new(
            FailureKind::Transient,
            format!("Nous refresh client could not be built: {error}"),
        )
    })
}

fn clean_url(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .map(|value| value.trim_end_matches('/'))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn validate_stored_portal_url(value: Option<&str>) -> Option<String> {
    let value = clean_url(value)?;
    let parsed = reqwest::Url::parse(&value).ok()?;
    let host = parsed.host_str()?.trim_end_matches('.').to_lowercase();
    let loopback = parsed.scheme() == "http" && matches!(host.as_str(), "localhost" | "127.0.0.1");
    ((parsed.scheme() == "https" && host == "portal.nousresearch.com") || loopback).then_some(value)
}

fn validate_network_inference_url(value: Option<&str>) -> Option<String> {
    let value = clean_url(value)?;
    let parsed = reqwest::Url::parse(&value).ok()?;
    (parsed.scheme() == "https"
        && parsed.host_str().is_some_and(|host| {
            host.trim_end_matches('.')
                .eq_ignore_ascii_case("inference-api.nousresearch.com")
        }))
    .then_some(value)
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn decode_claims(token: &str) -> Map<String, Value> {
    let Some(payload) = token
        .split('.')
        .nth(1)
        .filter(|_| token.matches('.').count() == 2)
    else {
        return Map::new();
    };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload));
    decoded
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn scope_values(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) => value
            .replace(',', " ")
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        Some(Value::Array(values)) => values
            .iter()
            .flat_map(|value| scope_values(Some(value)))
            .collect(),
        _ => Vec::new(),
    }
}

fn invoke_jwt_status(token: &str, state: &Map<String, Value>, now: f64) -> Option<&'static str> {
    let claims = decode_claims(token);
    if claims.is_empty() {
        return Some("access_token_not_jwt");
    }
    let scopes = scope_values(state.get("scope"))
        .into_iter()
        .chain(scope_values(claims.get("scope")))
        .chain(scope_values(claims.get("scp")));
    if !scopes.into_iter().any(|scope| scope == INVOKE_SCOPE) {
        return Some("missing_inference_invoke_scope");
    }
    if let Some(expiry) = claims.get("exp").and_then(Value::as_f64) {
        return (expiry <= now + REFRESH_SKEW_SECONDS).then_some("invoke_jwt_expiring");
    }
    let expiry = state
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|value| crate::message_timestamps::parse_iso_string(value, None));
    (expiry.is_none_or(|expiry| expiry <= now + REFRESH_SKEW_SECONDS))
        .then_some("invoke_jwt_expiry_unknown_or_expiring")
}

fn select_usable_token(state: &Map<String, Value>, now: f64) -> Option<String> {
    [
        ("agent_key", "agent_key_expires_at"),
        ("access_token", "expires_at"),
    ]
    .into_iter()
    .find_map(|(token_key, expiry_key)| {
        let token = state.get(token_key)?.as_str()?.trim();
        if token.is_empty() {
            return None;
        }
        let mut view = state.clone();
        if let Some(expiry) = state.get(expiry_key) {
            view.insert("expires_at".into(), expiry.clone());
        }
        invoke_jwt_status(token, &view, now)
            .is_none()
            .then(|| token.to_owned())
    })
}

fn install_routing_metadata(
    state: &mut Map<String, Value>,
    portal_url: &str,
    stored_inference_url: &str,
    client_id: &str,
) -> bool {
    let before = state.clone();
    state.insert("portal_base_url".into(), Value::String(portal_url.into()));
    state.insert(
        "inference_base_url".into(),
        Value::String(stored_inference_url.into()),
    );
    state.insert("client_id".into(), Value::String(client_id.into()));
    *state != before
}

fn install_runtime_aliases(state: &mut Map<String, Value>, token: &str) -> bool {
    let now = chrono::Utc::now();
    let claims = decode_claims(token);
    let expires_at = claims
        .get("exp")
        .and_then(Value::as_f64)
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds as i64, 0))
        .map(|timestamp| timestamp.to_rfc3339())
        .or_else(|| {
            state
                .get("expires_at")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let before = state.clone();
    let same_token = state.get("agent_key").and_then(Value::as_str) == Some(token);
    let existing_obtained_at = state
        .get("agent_key_obtained_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let expires_in = expires_at
        .as_deref()
        .and_then(|value| crate::message_timestamps::parse_iso_string(value, None))
        .map(|epoch| (epoch - now.timestamp() as f64).max(0.0) as i64)
        .or_else(|| {
            crate::python_value::integer(state.get("expires_in").unwrap_or(&Value::Null))
                .and_then(|value| value.as_i64())
                .map(|value| value.max(0))
        })
        .unwrap_or(0);
    state.insert("agent_key".into(), Value::String(token.to_owned()));
    state.insert("agent_key_id".into(), Value::Null);
    state.insert("agent_key_reused".into(), Value::Bool(false));
    if let Some(expires_at) = expires_at {
        state.insert("expires_at".into(), Value::String(expires_at.clone()));
        state.insert("agent_key_expires_at".into(), Value::String(expires_at));
    } else {
        state.insert("agent_key_expires_at".into(), Value::Null);
    }
    state.insert("expires_in".into(), json!(expires_in));
    state.insert("agent_key_expires_in".into(), json!(expires_in));
    state.insert(
        "agent_key_obtained_at".into(),
        Value::String(
            if same_token {
                existing_obtained_at
            } else {
                None
            }
            .unwrap_or_else(|| now.to_rfc3339()),
        ),
    );
    effective_state(&before) != effective_state(state)
}

fn effective_state(state: &Map<String, Value>) -> Map<String, Value> {
    state
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "expires_in" | "agent_key_expires_in"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn has_auth_material(state: &Map<String, Value>) -> bool {
    ["agent_key", "access_token", "refresh_token"]
        .into_iter()
        .any(|key| {
            state
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
        })
}

fn persist_state(
    transaction: &mut crate::auth_store::ProviderStateTransaction,
    state: &Map<String, Value>,
) -> Result<(), Failure> {
    let store = transaction.store_mut();
    let root = store.as_object_mut().ok_or_else(|| {
        Failure::new(FailureKind::Persistence, "Nous auth store is not an object")
    })?;
    let providers = root
        .entry("providers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            Failure::new(
                FailureKind::Persistence,
                "Nous provider store is not an object",
            )
        })?;
    providers.insert(PROVIDER.into(), Value::Object(state.clone()));
    root.insert("active_provider".into(), Value::String(PROVIDER.into()));
    sync_pool(root, state);
    transaction.commit().map_err(|error| {
        Failure::new(
            FailureKind::Persistence,
            format!("Nous rotated credentials could not be persisted: {error}"),
        )
    })
}

fn sync_pool(root: &mut Map<String, Value>, state: &Map<String, Value>) {
    let pool = root
        .entry("credential_pool")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    let Some(pool) = pool else { return };
    let rows = pool
        .entry(PROVIDER)
        .or_insert_with(|| json!([]))
        .as_array_mut();
    let Some(rows) = rows else { return };
    if !has_auth_material(state) {
        rows.retain(|entry| {
            !matches!(
                entry.get("source").and_then(Value::as_str),
                Some("device_code" | "manual:device_code")
            )
        });
        return;
    }
    let first = rows
        .iter()
        .position(|entry| entry.get("source").and_then(Value::as_str) == Some(DEVICE_CODE_SOURCE));
    let mut row = first
        .and_then(|index| rows.get(index))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let token_changed = state
        .get("access_token")
        .and_then(Value::as_str)
        .is_some_and(|token| row.get("access_token").and_then(Value::as_str) != Some(token));
    row.entry("id").or_insert_with(|| {
        Value::String(
            crate::install_identity::mint_id()
                .map(|id| id[..6].to_owned())
                .unwrap_or_else(|| "nous-oauth".into()),
        )
    });
    row.entry("priority").or_insert_with(|| {
        json!(
            rows.iter()
                .filter_map(|entry| entry["priority"].as_i64())
                .max()
                .unwrap_or(-1)
                + 1
        )
    });
    row.entry("label").or_insert_with(|| {
        Value::String(
            state
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| jwt_label(state))
                .unwrap_or_else(|| DEVICE_CODE_SOURCE.into()),
        )
    });
    row.insert("source".into(), Value::String(DEVICE_CODE_SOURCE.into()));
    row.insert("auth_type".into(), Value::String("oauth".into()));
    for key in [
        "access_token",
        "refresh_token",
        "expires_at",
        "token_type",
        "scope",
        "client_id",
        "portal_base_url",
        "inference_base_url",
        "agent_key",
        "agent_key_expires_at",
        "obtained_at",
        "expires_in",
        "agent_key_id",
        "agent_key_expires_in",
        "agent_key_reused",
        "agent_key_obtained_at",
        "tls",
    ] {
        if let Some(value) = state.get(key).filter(|value| !value.is_null()) {
            row.insert(key.into(), value.clone());
        }
    }
    if token_changed {
        for key in [
            "last_status",
            "last_status_at",
            "last_error_code",
            "last_error_reason",
            "last_error_message",
            "last_error_reset_at",
        ] {
            row.insert(key.into(), Value::Null);
        }
    }
    rows.retain(|entry| entry.get("source").and_then(Value::as_str) != Some(DEVICE_CODE_SOURCE));
    let index = first.unwrap_or(rows.len()).min(rows.len());
    rows.insert(index, Value::Object(row));
}

fn quarantine(state: &mut Map<String, Value>, code: &str) {
    for key in [
        "access_token",
        "refresh_token",
        "expires_at",
        "expires_in",
        "obtained_at",
        "agent_key",
        "agent_key_id",
        "agent_key_expires_at",
        "agent_key_expires_in",
        "agent_key_reused",
        "agent_key_obtained_at",
    ] {
        state.shift_remove(key);
    }
    state.insert(
        "last_auth_error".into(),
        json!({
            "provider":PROVIDER,
            "code":code,
            "message":format!("Nous Portal refresh failed ({code})"),
            "reason":"runtime_access_refresh_failure",
            "relogin_required":true,
            "at":chrono::Utc::now().to_rfc3339(),
        }),
    );
}

fn runtime_credential(
    store: &Value,
    token: String,
    base_url: String,
) -> crate::credential_pool::RuntimeCredential {
    let id = store["credential_pool"][PROVIDER]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["source"] == DEVICE_CODE_SOURCE))
        .and_then(|row| row["id"].as_str())
        .unwrap_or(DEVICE_CODE_SOURCE)
        .to_owned();
    crate::credential_pool::RuntimeCredential::new(id, token, Some(base_url))
}

fn jwt_label(state: &Map<String, Value>) -> Option<String> {
    let claims = state
        .get("access_token")
        .and_then(Value::as_str)
        .map(decode_claims)?;
    ["email", "preferred_username", "upn"]
        .into_iter()
        .find_map(|key| {
            claims
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

fn read_json(path: &Path) -> std::io::Result<Option<Value>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
            Ok(serde_json::from_slice(bytes).ok())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn merge_shared_state(state: &mut Map<String, Value>, shared: &Value) -> bool {
    let Some(shared) = shared.as_object() else {
        return false;
    };
    let Some(refresh) = shared
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    if shared
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_none_or(str::is_empty)
    {
        return false;
    }
    let local_refresh = state
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let shared_expiry = shared
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|value| crate::message_timestamps::parse_iso_string(value, None))
        .unwrap_or(0.0);
    let local_expiry = state
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|value| crate::message_timestamps::parse_iso_string(value, None))
        .unwrap_or(0.0);
    if refresh == local_refresh && shared_expiry <= local_expiry {
        return false;
    }
    for key in [
        "access_token",
        "refresh_token",
        "token_type",
        "scope",
        "client_id",
        "portal_base_url",
        "inference_base_url",
        "obtained_at",
        "expires_at",
    ] {
        if let Some(value) = shared
            .get(key)
            .filter(|value| crate::python_value::truthy(value))
        {
            state.insert(key.into(), value.clone());
        }
    }
    true
}

fn write_shared(path: &Path, state: &Map<String, Value>) -> std::io::Result<()> {
    if state
        .get("refresh_token")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || state
            .get("access_token")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Ok(());
    }
    let shared = json!({
        "_schema":1,
        "access_token":state.get("access_token"),
        "refresh_token":state.get("refresh_token"),
        "token_type":state.get("token_type").cloned().unwrap_or_else(|| json!("Bearer")),
        "scope":state.get("scope").cloned().unwrap_or_else(|| json!(INVOKE_SCOPE)),
        "client_id":state.get("client_id").cloned().unwrap_or_else(|| json!(DEFAULT_CLIENT_ID)),
        "portal_base_url":state.get("portal_base_url").cloned().unwrap_or_else(|| json!(DEFAULT_PORTAL_URL)),
        "inference_base_url":state.get("inference_base_url").cloned().unwrap_or_else(|| json!(DEFAULT_INFERENCE_URL)),
        "obtained_at":state.get("obtained_at"),
        "expires_at":state.get("expires_at"),
        "updated_at":chrono::Utc::now().to_rfc3339(),
    });
    let mut bytes = serde_json::to_vec_pretty(&shared)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');
    crate::atomic_file::write_private_replacing_symlink(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(exp: i64, scope: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({"exp":exp,"scope":scope})).unwrap());
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn invoke_jwt_requires_scope_and_two_minute_life() {
        let now = 1_700_000_000.0;
        let state = Map::from_iter([("scope".into(), json!(INVOKE_SCOPE))]);
        assert_eq!(
            invoke_jwt_status(&jwt(1_700_000_121, ""), &state, now),
            None
        );
        assert_eq!(
            invoke_jwt_status(&jwt(1_700_000_120, ""), &state, now),
            Some("invoke_jwt_expiring")
        );
        assert_eq!(
            invoke_jwt_status(&jwt(1_700_001_000, "profile"), &Map::new(), now),
            Some("missing_inference_invoke_scope")
        );
    }

    #[test]
    fn network_urls_are_allowlisted_but_operator_overrides_are_cleaned_only() {
        assert_eq!(
            validate_network_inference_url(Some("https://inference-api.nousresearch.com/v1/")),
            Some(DEFAULT_INFERENCE_URL.into())
        );
        assert_eq!(
            validate_network_inference_url(Some("https://evil.example/v1")),
            None
        );
        assert_eq!(
            validate_stored_portal_url(Some("http://127.0.0.1:9000/")),
            Some("http://127.0.0.1:9000".into())
        );
        assert_eq!(
            clean_url(Some("http://operator.invalid/v1/")),
            Some("http://operator.invalid/v1".into())
        );
    }

    #[test]
    fn partial_shared_state_without_access_token_is_ignored() {
        let mut local = Map::from_iter([("refresh_token".into(), json!("local-refresh"))]);
        assert!(!merge_shared_state(
            &mut local,
            &json!({"refresh_token":"peer-refresh"})
        ));
        assert_eq!(local["refresh_token"], "local-refresh");
    }

    #[cfg(unix)]
    #[test]
    fn shared_write_replaces_symlink_instead_of_following_it() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "hermes-nous-shared-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let target = dir.join("target.json");
        let shared = dir.join("nous_auth.json");
        std::fs::write(&target, b"original").unwrap();
        symlink(&target, &shared).unwrap();
        let state = Map::from_iter([
            ("access_token".into(), json!("access")),
            ("refresh_token".into(), json!("refresh")),
        ]);

        write_shared(&shared, &state).unwrap();

        assert!(!std::fs::symlink_metadata(&shared)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
    }

    #[tokio::test]
    async fn valid_token_persists_portal_override_without_persisting_inference_override() {
        let dir = std::env::temp_dir().join(format!(
            "hermes-nous-routing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let auth = dir.join("auth.json");
        let token = jwt(chrono::Utc::now().timestamp() + 3_600, INVOKE_SCOPE);
        std::fs::write(
            &auth,
            serde_json::to_vec(&json!({
                "active_provider":"openrouter",
                "providers":{"nous":{
                    "access_token":token,
                    "refresh_token":"refresh",
                    "scope":INVOKE_SCOPE,
                    "inference_base_url":DEFAULT_INFERENCE_URL,
                    "label":"My Nous Login"
                }}
            }))
            .unwrap(),
        )
        .unwrap();
        let locator = Locator::new(
            auth.clone(),
            None,
            dir.join("shared/nous_auth.json"),
            Some("http://127.0.0.1:9876/".into()),
            Some("http://operator.invalid/v1/".into()),
            Duration::from_secs(1),
        );

        let runtime = locator.resolve(false, None).await.unwrap();
        assert_eq!(runtime.api_key(), token);
        assert_eq!(runtime.base_url(), Some("http://operator.invalid/v1"));
        let store = crate::auth_store::load(&auth).unwrap();
        assert_eq!(store["active_provider"], "nous");
        assert_eq!(
            store["providers"]["nous"]["portal_base_url"],
            "http://127.0.0.1:9876"
        );
        assert_eq!(
            store["providers"]["nous"]["inference_base_url"],
            DEFAULT_INFERENCE_URL
        );
        assert_eq!(
            store["providers"]["nous"]["agent_key_expires_in"]
                .as_i64()
                .unwrap(),
            store["providers"]["nous"]["expires_in"].as_i64().unwrap()
        );
        assert_eq!(
            store["credential_pool"]["nous"][0]["label"],
            "My Nous Login"
        );
        assert_eq!(
            store["credential_pool"]["nous"][0]["id"]
                .as_str()
                .unwrap()
                .len(),
            6
        );
    }

    #[tokio::test]
    async fn unusable_token_without_refresh_is_unavailable_without_quarantine() {
        let dir = std::env::temp_dir().join(format!(
            "hermes-nous-no-refresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let auth = dir.join("auth.json");
        let expired = jwt(chrono::Utc::now().timestamp() - 1, INVOKE_SCOPE);
        std::fs::write(
            &auth,
            serde_json::to_vec(&json!({"providers":{"nous":{
                "access_token":expired,
                "scope":INVOKE_SCOPE
            }}}))
            .unwrap(),
        )
        .unwrap();
        let locator = Locator::new(
            auth.clone(),
            None,
            dir.join("shared/nous_auth.json"),
            None,
            None,
            Duration::from_secs(1),
        );

        let error = match locator.resolve(false, None).await {
            Ok(_) => panic!("unusable token unexpectedly resolved a credential"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), FailureKind::Unavailable);
        let store = crate::auth_store::load(&auth).unwrap();
        assert_eq!(store["providers"]["nous"]["access_token"], expired);
        assert!(store["providers"]["nous"]["last_auth_error"].is_null());
    }

    #[tokio::test]
    async fn routing_heal_does_not_resurrect_same_token_pool_cooldown() {
        let dir = std::env::temp_dir().join(format!(
            "hermes-nous-status-preservation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let auth = dir.join("auth.json");
        let token = jwt(chrono::Utc::now().timestamp() + 3_600, INVOKE_SCOPE);
        std::fs::write(
            &auth,
            serde_json::to_vec(&json!({
                "providers":{"nous":{
                    "access_token":token,
                    "refresh_token":"refresh",
                    "scope":INVOKE_SCOPE,
                    "portal_base_url":"https://poison.invalid"
                }},
                "credential_pool":{"nous":[{
                    "id":"same-token",
                    "source":"device_code",
                    "auth_type":"oauth",
                    "priority":0,
                    "access_token":token,
                    "last_status":"exhausted",
                    "last_error_code":"rate_limit"
                }]}
            }))
            .unwrap(),
        )
        .unwrap();
        let locator = Locator::new(
            auth.clone(),
            None,
            dir.join("shared/nous_auth.json"),
            None,
            None,
            Duration::from_secs(1),
        );

        locator.resolve(false, None).await.unwrap();
        let store = crate::auth_store::load(&auth).unwrap();
        assert_eq!(
            store["providers"]["nous"]["portal_base_url"],
            DEFAULT_PORTAL_URL
        );
        assert_eq!(
            store["credential_pool"]["nous"][0]["last_status"],
            "exhausted"
        );
        assert_eq!(
            store["credential_pool"]["nous"][0]["last_error_code"],
            "rate_limit"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rotated_token_is_not_returned_when_auth_commit_fails() {
        use axum::{routing::post, Json, Router};
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "hermes-nous-persist-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source_dir = dir.join("source");
        let profile_dir = dir.join("profile");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let target = source_dir.join("auth.json");
        let profile = profile_dir.join("auth.json");
        let expired = jwt(chrono::Utc::now().timestamp() - 1, INVOKE_SCOPE);
        std::fs::write(
            &target,
            serde_json::to_vec(&json!({"providers":{"nous":{
                "access_token":expired,
                "refresh_token":"single-use-refresh",
                "scope":INVOKE_SCOPE
            }}}))
            .unwrap(),
        )
        .unwrap();
        symlink(&target, &profile).unwrap();

        let replacement = jwt(chrono::Utc::now().timestamp() + 3_600, INVOKE_SCOPE);
        let sabotaged_parent = source_dir.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let application = Router::new().route(
            "/api/oauth/token",
            post(move || {
                let replacement = replacement.clone();
                let target = target.clone();
                let sabotaged_parent = sabotaged_parent.clone();
                async move {
                    std::fs::remove_file(target).unwrap();
                    std::fs::remove_dir(&sabotaged_parent).unwrap();
                    std::fs::write(&sabotaged_parent, b"not a directory").unwrap();
                    Json(json!({
                        "access_token":replacement,
                        "refresh_token":"rotated-refresh",
                        "expires_in":3600,
                        "scope":INVOKE_SCOPE
                    }))
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, application).await.unwrap();
        });
        let locator = Locator::new(
            profile,
            None,
            dir.join("shared/nous_auth.json"),
            Some(format!("http://{address}")),
            None,
            Duration::from_secs(2),
        );

        let error = match locator.resolve(false, None).await {
            Ok(_) => panic!("unpersisted rotated token unexpectedly escaped"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), FailureKind::Persistence);
        assert!(!dir.join("shared/nous_auth.json").exists());
        server.abort();
    }

    #[tokio::test]
    async fn forced_refresh_adopts_peer_rotation_from_root_without_posting() {
        use axum::{routing::post, Router};
        use std::sync::{Arc, Mutex};

        let calls = Arc::new(Mutex::new(0usize));
        let server_calls = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let application = Router::new().route(
            "/api/oauth/token",
            post(move || {
                let server_calls = server_calls.clone();
                async move {
                    *server_calls.lock().unwrap() += 1;
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "unexpected")
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, application).await.unwrap();
        });

        let dir = std::env::temp_dir().join(format!(
            "hermes-nous-peer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = dir.join("profiles/red/auth.json");
        let root = dir.join("auth.json");
        std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
        std::fs::write(&profile, br#"{"version":1,"providers":{}}"#).unwrap();
        let token = jwt(chrono::Utc::now().timestamp() + 3_600, INVOKE_SCOPE);
        std::fs::write(
            &root,
            serde_json::to_vec(&json!({
                "providers":{"nous":{
                    "access_token":token,
                    "agent_key":"stale-token",
                    "refresh_token":"peer-refresh",
                    "scope":INVOKE_SCOPE,
                    "expires_at":(chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                }}
            }))
            .unwrap(),
        )
        .unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let locator = Locator::new(
            profile.clone(),
            Some(root.clone()),
            dir.join("shared/nous_auth.json"),
            Some(format!("http://{address}")),
            Some("http://127.0.0.1:1".into()),
            Duration::from_secs(2),
        );
        let runtime = locator.resolve(true, Some("stale-token")).await.unwrap();
        assert_eq!(runtime.api_key(), token);
        assert_eq!(*calls.lock().unwrap(), 0);
        assert!(crate::auth_store::load(&profile).unwrap()["providers"]["nous"].is_null());
        let root_store = crate::auth_store::load(&root).unwrap();
        assert_eq!(root_store["providers"]["nous"]["agent_key"], token);
        assert_eq!(
            root_store["credential_pool"]["nous"][0]["source"],
            DEVICE_CODE_SOURCE
        );
        server.abort();
    }

    #[tokio::test]
    async fn sibling_profiles_post_each_single_use_refresh_token_once() {
        use axum::{routing::post, Json, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let replacement = jwt(chrono::Utc::now().timestamp() + 3_600, INVOKE_SCOPE);
        let response_token = replacement.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let application = Router::new().route(
            "/api/oauth/token",
            post(move || {
                let token = response_token.clone();
                let server_calls = server_calls.clone();
                async move {
                    server_calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "access_token":token,
                        "refresh_token":"refresh-two",
                        "expires_in":3600,
                        "scope":INVOKE_SCOPE
                    }))
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, application).await.unwrap();
        });

        let dir = std::env::temp_dir().join(format!(
            "hermes-nous-cross-profile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let first_auth = dir.join("profiles/first/auth.json");
        let second_auth = dir.join("profiles/second/auth.json");
        std::fs::create_dir_all(first_auth.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second_auth.parent().unwrap()).unwrap();
        let old_token = jwt(chrono::Utc::now().timestamp() + 7_200, INVOKE_SCOPE);
        let state = json!({"providers":{"nous":{
            "access_token":old_token,
            "agent_key":old_token,
            "refresh_token":"refresh-one",
            "scope":INVOKE_SCOPE
        }}});
        let bytes = serde_json::to_vec(&state).unwrap();
        std::fs::write(&first_auth, &bytes).unwrap();
        std::fs::write(&second_auth, &bytes).unwrap();
        let shared = dir.join("shared/nous_auth.json");
        let locator = |auth| {
            Locator::new(
                auth,
                None,
                shared.clone(),
                Some(format!("http://{address}")),
                None,
                Duration::from_secs(2),
            )
        };
        let first = locator(first_auth.clone());
        let second = locator(second_auth.clone());

        let (first_result, second_result) = tokio::join!(
            first.resolve(true, Some(&old_token)),
            second.resolve(true, Some(&old_token))
        );
        assert_eq!(first_result.unwrap().api_key(), replacement);
        assert_eq!(second_result.unwrap().api_key(), replacement);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            crate::auth_store::load(&first_auth).unwrap()["providers"]["nous"]["refresh_token"],
            "refresh-two"
        );
        assert_eq!(
            crate::auth_store::load(&second_auth).unwrap()["providers"]["nous"]["refresh_token"],
            "refresh-two"
        );
        server.abort();
    }

    #[tokio::test]
    async fn terminal_refresh_quarantines_singleton_pool_and_shared_copy() {
        use axum::{routing::post, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let application = Router::new().route(
            "/api/oauth/token",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error":"refresh_token_reused",
                        "error_description":"reuse detected"
                    })),
                )
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, application).await.unwrap();
        });

        let dir = std::env::temp_dir().join(format!(
            "hermes-nous-terminal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("shared")).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let auth = dir.join("auth.json");
        let shared = dir.join("shared/nous_auth.json");
        let expired = jwt(chrono::Utc::now().timestamp() - 1, INVOKE_SCOPE);
        let state = json!({
            "access_token":expired,
            "refresh_token":"spent-refresh",
            "scope":INVOKE_SCOPE,
            "expires_at":(chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
            "inference_base_url":DEFAULT_INFERENCE_URL,
        });
        std::fs::write(
            &auth,
            serde_json::to_vec(&json!({
                "providers":{"nous":state},
                "credential_pool":{"nous":[
                    {"id":"oauth","source":"device_code","access_token":expired},
                    {"id":"legacy","source":"manual:device_code","access_token":expired},
                    {"id":"manual","source":"manual","access_token":"independent"}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(&shared, serde_json::to_vec(&state).unwrap()).unwrap();
        let locator = Locator::new(
            auth.clone(),
            None,
            shared.clone(),
            Some(format!("http://{address}")),
            Some("http://127.0.0.1:1".into()),
            Duration::from_secs(2),
        );
        let error = match locator.resolve(false, None).await {
            Ok(_) => panic!("terminal refresh unexpectedly resolved a credential"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), FailureKind::Terminal);
        let store = crate::auth_store::load(&auth).unwrap();
        assert!(store["providers"]["nous"]["access_token"].is_null());
        assert_eq!(
            store["providers"]["nous"]["last_auth_error"]["code"],
            "refresh_token_reused"
        );
        assert_eq!(
            store["credential_pool"]["nous"].as_array().unwrap().len(),
            1
        );
        assert_eq!(store["credential_pool"]["nous"][0]["id"], "manual");
        assert!(!shared.exists());
        server.abort();
    }
}
