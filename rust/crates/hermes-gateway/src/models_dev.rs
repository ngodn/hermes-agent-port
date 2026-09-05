//! Cloud model registry cache, ported from agent/models_dev.py.
//! Keep refresh state shared by callers; model lookup and overrides consume the
//! returned immutable registry. Offline fetches never schedule network work.
#![allow(dead_code)]
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TTL: f64 = 4.0 * 3600.0;
const RETRY: f64 = 300.0;

struct CacheState {
    registry: Arc<Value>,
    updated: f64,
    retry_after: f64,
    refreshing: bool,
}

pub struct ModelsDev {
    root: PathBuf,
    url: String,
    client: Option<reqwest::Client>,
    state: Mutex<CacheState>,
    fetch_lock: tokio::sync::Mutex<()>,
}

enum Fetched {
    Registry(Value, String),
    NotModified,
}

impl ModelsDev {
    pub fn new(root: PathBuf, config: &Value) -> Arc<Self> {
        let url = config
            .get("models_dev")
            .and_then(|v| v.get("url"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("https://models.dev/api.json")
            .to_owned();
        Arc::new(Self {
            root,
            url,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .read_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::limited(30))
                .build()
                .ok(),
            state: Mutex::new(CacheState {
                registry: Arc::new(json!({})),
                updated: 0.0,
                retry_after: 0.0,
                refreshing: false,
            }),
            fetch_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn snapshot(&self) -> Arc<Value> {
        Arc::clone(&self.state.lock().unwrap().registry)
    }
    fn cache_path(&self) -> PathBuf {
        self.root.join("models_dev_cache.json")
    }
    fn etag_path(&self) -> PathBuf {
        self.root.join("models_dev_cache.etag")
    }
    fn clear_etag(&self) {
        let _ = std::fs::remove_file(self.etag_path());
    }

    fn disk_age(&self) -> Option<f64> {
        SystemTime::now()
            .duration_since(std::fs::metadata(self.cache_path()).ok()?.modified().ok()?)
            .ok()
            .map(|age| age.as_secs_f64())
    }

    fn load_disk(&self) -> Option<Value> {
        let path = self.cache_path();
        if !path.exists() {
            return None;
        }
        match std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(value) if valid(&value) => Some(value),
            _ => {
                tracing::warn!("models.dev disk cache is unreadable or invalid; quarantining");
                let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                self.clear_etag();
                None
            }
        }
    }

    fn hydrate(&self, data: Value, updated: f64) {
        let mut state = self.state.lock().unwrap();
        // Disk I/O may overlap a successful refresh; never replace its result.
        if !valid(&state.registry) {
            state.registry = Arc::new(data);
            state.updated = updated;
        }
    }

    fn grace(&self) {
        let mut state = self.state.lock().unwrap();
        state.updated = state.updated.max(now() - TTL + RETRY);
    }

    /// Memory, disk, then serialized network. Force bypasses cache freshness and
    /// failure backoff, but an explicit offline request always wins over force.
    pub async fn fetch(self: &Arc<Self>, force: bool, allow_network: bool) -> Arc<Value> {
        if !allow_network {
            if !valid(&self.snapshot()) {
                if let Some(data) = self.load_disk() {
                    self.hydrate(data, self.disk_age().map_or(0.0, |age| now() - age));
                }
            }
            return self.snapshot();
        }
        if !force {
            let stale = {
                let state = self.state.lock().unwrap();
                if valid(&state.registry) && now() - state.updated < TTL {
                    return Arc::clone(&state.registry);
                }
                valid(&state.registry)
            };
            if stale {
                self.grace();
                self.start_background();
                return self.snapshot();
            }
            if let Some(age) = self.disk_age() {
                if let Some(data) = self.load_disk() {
                    self.hydrate(data, if age < TTL { now() - age } else { 0.0 });
                    if age >= TTL {
                        self.grace();
                        self.start_background();
                    }
                    return self.snapshot();
                }
            }
            if now() < self.state.lock().unwrap().retry_after {
                return self.snapshot();
            }
        }
        let _fetch = self.fetch_lock.lock().await;
        if !force {
            let state = self.state.lock().unwrap();
            if valid(&state.registry) || now() < state.retry_after {
                return Arc::clone(&state.registry);
            }
        }
        if force && !valid(&self.snapshot()) {
            if let Some(data) = self.load_disk() {
                self.hydrate(data, 0.0);
            }
        }
        let failed = self.refresh_locked().await;
        if failed && !valid(&self.snapshot()) {
            if let Some(data) = self.load_disk() {
                self.hydrate(data, 0.0);
            }
        }
        self.snapshot()
    }

    fn start_background(self: &Arc<Self>) {
        {
            let mut state = self.state.lock().unwrap();
            if state.refreshing || now() < state.retry_after {
                return;
            }
            state.refreshing = true;
        }
        let catalog = Arc::clone(self);
        tokio::spawn(async move {
            let _fetch = catalog.fetch_lock.lock().await;
            catalog.refresh_locked().await;
            catalog.state.lock().unwrap().refreshing = false;
        });
    }

    /// The fetch mutex spans HTTP and commit, so foreground and background
    /// requests cannot overwrite each other's ETag, registry, or retry state.
    async fn refresh_locked(&self) -> bool {
        match self.request().await {
            Some(Fetched::Registry(data, etag)) => {
                if let Ok(bytes) = serde_json::to_vec(&data) {
                    let _ = crate::atomic_file::write(&self.cache_path(), &bytes);
                }
                // Python retains an earlier sidecar when the response omits ETag.
                if !etag.is_empty() {
                    let _ = crate::atomic_file::write(&self.etag_path(), etag.as_bytes());
                }
                let mut state = self.state.lock().unwrap();
                state.registry = Arc::new(data);
                state.updated = now();
                state.retry_after = 0.0;
            }
            Some(Fetched::NotModified) => {
                let mut state = self.state.lock().unwrap();
                if valid(&state.registry) {
                    state.updated = now();
                    state.retry_after = 0.0;
                } else {
                    self.clear_etag();
                    state.retry_after = now() + RETRY;
                }
                // A 304 deliberately leaves the disk body and mtime untouched.
            }
            None => {
                self.state.lock().unwrap().retry_after = now() + RETRY;
                return true;
            }
        }
        false
    }

    async fn request(&self) -> Option<Fetched> {
        let mut request = self.client.as_ref()?.get(&self.url);
        if valid(&self.snapshot()) {
            if let Ok(etag) = std::fs::read_to_string(self.etag_path()) {
                if !etag.trim().is_empty() {
                    request = request.header("if-none-match", etag.trim());
                }
            }
        }
        let response = request.send().await.ok()?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Some(Fetched::NotModified);
        }
        let response = response.error_for_status().ok()?;
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let data = response.json::<Value>().await.ok()?;
        valid(&data).then_some(Fetched::Registry(data, etag))
    }
}

fn valid(value: &Value) -> bool {
    value.as_object().is_some_and(|map| !map.is_empty())
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Capability fields consumed by routing and context selection. The source can
/// carry a non-string catalog family; only explicit overrides stringify it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelCapabilities {
    pub supports_reasoning: bool,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub model_family: Value,
}

fn provider_map() -> &'static [(String, String)] {
    static MAPPING: std::sync::LazyLock<Vec<(String, String)>> = std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../../../tools/models-dev-provider-map.json"))
            .expect("source provider mapping")
    });
    &MAPPING
}

fn mapped_provider(provider: &str) -> Option<&'static str> {
    provider_map()
        .iter()
        .find(|(id, _)| id == provider)
        .map(|(_, mapped)| mapped.as_str())
}

fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn provider_overrides<'a>(overrides: &'a Value, provider: &str) -> Option<&'a Value> {
    let provider = provider.trim_matches(python_whitespace);
    if provider.is_empty() {
        return None;
    }
    let mut candidates = vec![provider];
    if let Some(mapped) = mapped_provider(provider) {
        candidates.push(mapped);
    }
    candidates.extend(
        provider_map()
            .iter()
            .filter(|(_, mapped)| mapped == provider)
            .map(|(id, _)| id.as_str()),
    );
    candidates
        .into_iter()
        .filter_map(|id| overrides.get(id))
        .find(|section| section.is_object())
}

fn explicit_override<'a>(section: &'a Value, model: &str) -> Option<&'a Value> {
    let model = model.trim_matches(python_whitespace);
    if model.is_empty() {
        return None;
    }
    if let Some(entry) = section.get(model).filter(|v| v.is_object()) {
        return Some(entry);
    }
    let lower = model.to_lowercase();
    section
        .as_object()?
        .iter()
        .find(|(id, value)| {
            id.as_str() != "_default" && id.to_lowercase() == lower && value.is_object()
        })
        .map(|(_, value)| value)
}

fn find_model<'a>(models: &'a Value, model: &str) -> Option<&'a Value> {
    let models = models.as_object()?;
    for suffix in ["", ":cloud", "-cloud"] {
        let key = format!("{model}{suffix}");
        if let Some(entry) = models.get(&key).filter(|v| v.is_object()) {
            return Some(entry);
        }
        let lower = key.to_lowercase();
        if let Some((_, entry)) = models
            .iter()
            .find(|(id, entry)| id.to_lowercase() == lower && entry.is_object())
        {
            return Some(entry);
        }
    }
    None
}

fn default_override<'a>(overrides: &'a Value, provider: &str) -> Option<&'a Value> {
    provider_overrides(overrides, provider)
        .and_then(|section| section.get("_default"))
        .filter(|v| v.is_object())
        .or_else(|| overrides.get("_default").filter(|v| v.is_object()))
}

fn explicit_context(config: &Value, provider: &str, model: &str) -> Option<u64> {
    provider_overrides(&config["model_overrides"], provider)
        .and_then(|section| explicit_override(section, model))
        .and_then(|patch| positive_override(patch, "context_window"))
}

/// Context lookup keeps looking when a matching model has no usable context.
/// Capability lookup instead treats that entry as known and keeps field defaults.
fn resolve_context(registry: &Value, config: &Value, provider: &str, model: &str) -> Option<u64> {
    if let Some(context) = explicit_context(config, provider, model) {
        return Some(context);
    }
    if let Some(models) = mapped_provider(provider)
        .and_then(|id| registry.get(id))
        .and_then(|p| p.get("models"))
        .and_then(Value::as_object)
    {
        for suffix in ["", ":cloud", "-cloud"] {
            let key = format!("{model}{suffix}");
            let lower = key.to_lowercase();
            let candidates = models.get(&key).into_iter().chain(
                models
                    .iter()
                    .filter(|(id, _)| id.to_lowercase() == lower)
                    .map(|(_, v)| v),
            );
            for entry in candidates {
                let context = catalog_limit(entry.get("limit").and_then(|v| v.get("context")), 0);
                if context > 0 {
                    return Some(context);
                }
            }
        }
    }
    default_override(&config["model_overrides"], provider)
        .and_then(|patch| positive_override(patch, "context_window"))
}

fn positive_override(value: &Value, key: &str) -> Option<u64> {
    let raw = value.get(key)?;
    if raw.is_null() {
        return None;
    }
    let number = crate::python_value::integer(raw)
        .and_then(|n| n.as_u64())
        .filter(|n| *n > 0);
    if number.is_none() {
        static WARNED: std::sync::LazyLock<Mutex<std::collections::HashSet<String>>> =
            std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
        let warning = format!("{key}:{}", crate::python_value::python_repr(raw));
        if WARNED.lock().unwrap().insert(warning) {
            tracing::warn!(field = key, value = %raw, "model_overrides: ignoring invalid positive integer");
        }
    }
    number
}

fn catalog_limit(value: Option<&Value>, fallback: u64) -> u64 {
    let Some(value) = value else {
        return fallback;
    };
    let positive = match value {
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64().is_some_and(|n| n > 0.0),
        _ => false,
    };
    if positive {
        crate::python_value::integer(value)
            .and_then(|n| n.as_u64())
            .unwrap_or(fallback)
    } else {
        fallback
    }
}

/// Explicit overrides patch known models; defaults only fill catalog misses.
/// A suffix match counts as a hit, and an empty explicit override suppresses
/// defaults just as an empty Python dict does when selected by identity.
fn resolve_capabilities(
    registry: &Value,
    config: &Value,
    provider: &str,
    model: &str,
) -> Option<ModelCapabilities> {
    use crate::python_value::truthy;
    let entry = mapped_provider(provider)
        .and_then(|id| registry.get(id))
        .and_then(|p| p.get("models"))
        .and_then(|models| find_model(models, model));
    let overrides = config
        .get("model_overrides")
        .filter(|v| v.is_object())
        .unwrap_or(&Value::Null);
    let section = provider_overrides(overrides, provider);
    let explicit = section.and_then(|section| explicit_override(section, model));
    let patch = explicit.or_else(|| {
        if entry.is_some() {
            None
        } else {
            section
                .and_then(|section| section.get("_default"))
                .filter(|v| v.is_object())
                .or_else(|| overrides.get("_default").filter(|v| v.is_object()))
        }
    });
    if entry.is_none() && patch.is_none() {
        return None;
    }
    let mut result = ModelCapabilities {
        supports_tools: entry.is_none(),
        supports_reasoning: false,
        supports_vision: false,
        context_window: 200000,
        max_output_tokens: 8192,
        model_family: json!(""),
    };
    if let Some(entry) = entry {
        result.supports_tools = truthy(&entry["tool_call"]);
        result.supports_reasoning = truthy(&entry["reasoning"]);
        result.supports_vision = entry
            .get("modalities")
            .and_then(|m| m.get("input"))
            .and_then(Value::as_array)
            .map_or_else(
                || truthy(&entry["attachment"]),
                |inputs| inputs.contains(&json!("image")),
            );
        result.context_window =
            catalog_limit(entry.get("limit").and_then(|v| v.get("context")), 200000);
        result.max_output_tokens =
            catalog_limit(entry.get("limit").and_then(|v| v.get("output")), 8192);
        if truthy(&entry["family"]) {
            result.model_family = entry["family"].clone();
        }
    }
    if let Some(patch) = patch {
        for (field, target) in [
            ("supports_tools", &mut result.supports_tools),
            ("supports_reasoning", &mut result.supports_reasoning),
            ("supports_vision", &mut result.supports_vision),
        ] {
            if let Some(value) = patch.get(field) {
                *target = truthy(value);
            }
        }
        if let Some(value) = positive_override(patch, "context_window") {
            result.context_window = value;
        }
        if let Some(value) = positive_override(patch, "max_output_tokens") {
            result.max_output_tokens = value;
        }
        if let Some(value) = patch.get("model_family") {
            result.model_family = Value::String(if !truthy(value) {
                String::new()
            } else {
                match value {
                    Value::String(s) => s.clone(),
                    value => crate::python_value::python_repr(value),
                }
            });
        }
    }
    Some(result)
}

impl ModelsDev {
    pub async fn context_window(
        self: &Arc<Self>,
        provider: &str,
        model: &str,
        config: &Value,
        allow_network: bool,
    ) -> Option<u64> {
        if let Some(context) = explicit_context(config, provider, model) {
            return Some(context);
        }
        let registry = if mapped_provider(provider).is_some() {
            self.fetch(false, allow_network).await
        } else {
            Arc::new(json!({}))
        };
        resolve_context(&registry, config, provider, model)
    }
}

impl ModelsDev {
    pub async fn capabilities(
        self: &Arc<Self>,
        provider: &str,
        model: &str,
        config: &Value,
        allow_network: bool,
    ) -> Option<ModelCapabilities> {
        // Unknown providers may resolve solely through overrides. They must not
        // trigger a registry fetch just because network access was permitted.
        let registry = if mapped_provider(provider).is_some() {
            self.fetch(false, allow_network).await
        } else {
            Arc::new(json!({}))
        };
        resolve_capabilities(&registry, config, provider, model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
        Router,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::Notify;

    #[derive(Clone)]
    struct HttpState {
        response: Arc<Mutex<(StatusCode, String, String)>>,
        requests: Arc<Mutex<Vec<Option<String>>>>,
        entered: Arc<Notify>,
        release: Option<Arc<Notify>>,
    }
    struct Fixture {
        catalog: Arc<ModelsDev>,
        http: HttpState,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_dir_all(&self.catalog.root);
        }
    }
    async fn handler(State(state): State<HttpState>, headers: HeaderMap) -> impl IntoResponse {
        state.requests.lock().unwrap().push(
            headers
                .get("if-none-match")
                .map(|s| s.to_str().unwrap().to_owned()),
        );
        state.entered.notify_one();
        if let Some(release) = &state.release {
            release.notified().await;
        }
        let (status, body, etag) = state.response.lock().unwrap().clone();
        (status, [("etag", etag)], body)
    }
    async fn fixture(block: bool) -> Fixture {
        static ID: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "hermes-models-dev-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let catalog = ModelsDev::new(
            root,
            &json!({"models_dev": {"url": format!("http://{}/api.json", listener.local_addr().unwrap())}}),
        );
        let http = HttpState {
            response: Arc::new(Mutex::new((
                StatusCode::OK,
                json!({"provider": {"models": {"vision": {"attachment": true}}}}).to_string(),
                "\"v1\"".into(),
            ))),
            requests: Arc::new(Mutex::new(Vec::new())),
            entered: Arc::new(Notify::new()),
            release: block.then(|| Arc::new(Notify::new())),
        };
        let app = Router::new()
            .route("/api.json", get(handler))
            .with_state(http.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Fixture {
            catalog,
            http,
            server,
        }
    }
    fn write_disk(catalog: &ModelsDev, data: &Value, age: Duration) {
        std::fs::write(catalog.cache_path(), serde_json::to_vec(data).unwrap()).unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(catalog.cache_path())
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now() - age))
            .unwrap();
    }
    async fn background_done(catalog: &ModelsDev) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while catalog.state.lock().unwrap().refreshing {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cold_fetch_persists_then_memory_and_disk_avoid_network() {
        let f = fixture(false).await;
        let data = f.catalog.fetch(false, true).await;
        assert!(valid(&data));
        assert_eq!(f.http.requests.lock().unwrap().as_slice(), [None]);
        assert_eq!(
            std::fs::read_to_string(f.catalog.etag_path()).unwrap(),
            "\"v1\""
        );
        assert_eq!(f.catalog.fetch(false, true).await, data);
        let restarted = ModelsDev::new(f.catalog.root.clone(), &json!({}));
        assert_eq!(restarted.fetch(false, true).await, data);
        assert_eq!(f.http.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn offline_overrides_force_and_accepts_future_disk_mtime() {
        let f = fixture(false).await;
        let data = json!({"offline": {}});
        write_disk(&f.catalog, &data, Duration::ZERO);
        std::fs::File::options()
            .write(true)
            .open(f.catalog.cache_path())
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() + Duration::from_secs(600)),
            )
            .unwrap();
        assert_eq!(*f.catalog.fetch(true, false).await, data);
        assert_eq!(f.catalog.state.lock().unwrap().updated, 0.0);
        assert!(f.http.requests.lock().unwrap().is_empty());
        assert!(!f.catalog.state.lock().unwrap().refreshing);
    }

    #[tokio::test]
    async fn stale_disk_is_served_while_one_refresh_runs() {
        let f = fixture(true).await;
        let old = json!({"old": {}});
        write_disk(&f.catalog, &old, Duration::from_secs(TTL as u64 + 1));
        assert_eq!(*f.catalog.fetch(false, true).await, old);
        tokio::time::timeout(Duration::from_secs(3), f.http.entered.notified())
            .await
            .unwrap();
        for _ in 0..3 {
            assert_eq!(*f.catalog.fetch(false, true).await, old);
        }
        assert_eq!(f.http.requests.lock().unwrap().len(), 1);
        f.http.release.as_ref().unwrap().notify_one();
        background_done(&f.catalog).await;
        assert_ne!(*f.catalog.snapshot(), old);
    }

    #[tokio::test]
    async fn cold_force_hydrates_etag_and_304_preserves_disk_mtime() {
        let f = fixture(false).await;
        let old = json!({"old": {}});
        write_disk(&f.catalog, &old, Duration::from_secs(86400));
        std::fs::write(f.catalog.etag_path(), "  \"cached\" \n").unwrap();
        let modified = std::fs::metadata(f.catalog.cache_path())
            .unwrap()
            .modified()
            .unwrap();
        *f.http.response.lock().unwrap() = (StatusCode::NOT_MODIFIED, String::new(), String::new());
        assert_eq!(*f.catalog.fetch(true, true).await, old);
        assert_eq!(
            f.http.requests.lock().unwrap().as_slice(),
            [Some("\"cached\"".into())]
        );
        assert_eq!(
            std::fs::metadata(f.catalog.cache_path())
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        assert!(now() - f.catalog.state.lock().unwrap().updated < 5.0);
    }

    #[tokio::test]
    async fn corrupt_cache_quarantines_and_does_not_send_orphan_etag() {
        let f = fixture(false).await;
        for bytes in [b"broken".as_slice(), b"{}", b"[]"] {
            std::fs::write(f.catalog.cache_path(), bytes).unwrap();
            std::fs::write(f.catalog.etag_path(), "orphan").unwrap();
            f.catalog.state.lock().unwrap().registry = Arc::new(json!({}));
            assert!(valid(&*f.catalog.fetch(true, true).await));
            assert_eq!(
                std::fs::read(f.catalog.cache_path().with_extension("json.corrupt")).unwrap(),
                bytes
            );
        }
        assert_eq!(
            f.http.requests.lock().unwrap().as_slice(),
            [None, None, None]
        );
    }

    #[tokio::test]
    async fn failures_back_off_and_force_bypasses_backoff() {
        let f = fixture(false).await;
        *f.http.response.lock().unwrap() = (StatusCode::BAD_GATEWAY, String::new(), String::new());
        assert!(!valid(&*f.catalog.fetch(false, true).await));
        assert!(!valid(&*f.catalog.fetch(false, true).await));
        assert_eq!(f.http.requests.lock().unwrap().len(), 1);
        assert!(f.catalog.state.lock().unwrap().retry_after > now());
        *f.http.response.lock().unwrap() =
            (StatusCode::OK, "{\"recovered\":{}}".into(), String::new());
        assert!(valid(&*f.catalog.fetch(true, true).await));
        assert_eq!(f.catalog.state.lock().unwrap().retry_after, 0.0);
        assert_eq!(f.http.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unconditional_304_without_data_clears_etag_and_backs_off() {
        let f = fixture(false).await;
        std::fs::write(f.catalog.etag_path(), "orphan").unwrap();
        *f.http.response.lock().unwrap() = (StatusCode::NOT_MODIFIED, String::new(), String::new());
        assert!(!valid(&*f.catalog.fetch(false, true).await));
        assert!(!f.catalog.etag_path().exists());
        assert!(f.catalog.state.lock().unwrap().retry_after > now());
        assert_eq!(f.http.requests.lock().unwrap().as_slice(), [None]);
    }

    #[tokio::test]
    async fn concurrent_cold_fetches_share_one_request() {
        let f = fixture(true).await;
        let first = {
            let catalog = Arc::clone(&f.catalog);
            tokio::spawn(async move { catalog.fetch(false, true).await })
        };
        tokio::time::timeout(Duration::from_secs(3), f.http.entered.notified())
            .await
            .unwrap();
        let second = {
            let catalog = Arc::clone(&f.catalog);
            tokio::spawn(async move { catalog.fetch(false, true).await })
        };
        f.http.release.as_ref().unwrap().notify_one();
        let (a, b) = tokio::time::timeout(Duration::from_secs(3), async {
            (first.await.unwrap(), second.await.unwrap())
        })
        .await
        .unwrap();
        assert_eq!(a, b);
        assert_eq!(f.http.requests.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn cache_transitions_match_python() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/cloud-catalog-goldens.json"))
                .unwrap();
        for (index, case) in cases.iter().enumerate() {
            let f = fixture(false).await;
            match case["initial"].as_str().unwrap() {
                "missing" => {}
                "corrupt" => std::fs::write(f.catalog.cache_path(), b"broken").unwrap(),
                kind => {
                    write_disk(
                        &f.catalog,
                        &json!({"old": {}}),
                        Duration::from_secs(if kind == "stale" { 86400 } else { 60 }),
                    );
                    if kind == "future" {
                        std::fs::File::options()
                            .write(true)
                            .open(f.catalog.cache_path())
                            .unwrap()
                            .set_times(
                                std::fs::FileTimes::new()
                                    .set_modified(SystemTime::now() + Duration::from_secs(600)),
                            )
                            .unwrap();
                    }
                }
            }
            std::fs::write(f.catalog.etag_path(), "\"cached\"").unwrap();
            f.http.response.lock().unwrap().0 =
                StatusCode::from_u16(case["response"].as_u64().unwrap() as u16).unwrap();
            let returned = f
                .catalog
                .fetch(
                    case["force"].as_bool().unwrap(),
                    case["online"].as_bool().unwrap(),
                )
                .await;
            assert_eq!(*returned, case["returned"], "return case {index}: {case}");
            background_done(&f.catalog).await;
            assert_eq!(*f.catalog.snapshot(), case["final"], "final case {index}");
            assert_eq!(
                json!(*f.http.requests.lock().unwrap()),
                case["requests"],
                "requests case {index}"
            );
            assert_eq!(
                json!(f.catalog.state.lock().unwrap().retry_after > now()),
                case["backoff"],
                "backoff case {index}"
            );
            assert_eq!(
                json!(f
                    .catalog
                    .cache_path()
                    .with_extension("json.corrupt")
                    .exists()),
                case["quarantined"],
                "quarantine case {index}"
            );
            assert_eq!(
                json!(std::fs::read_to_string(f.catalog.etag_path()).ok()),
                case["etag"],
                "etag case {index}"
            );
        }
    }
    #[tokio::test]
    async fn forced_success_after_background_failure_clears_backoff() {
        let f = fixture(true).await;
        let old = json!({"old": {}});
        write_disk(&f.catalog, &old, Duration::from_secs(86400));
        *f.http.response.lock().unwrap() = (StatusCode::BAD_GATEWAY, String::new(), String::new());
        assert_eq!(*f.catalog.fetch(false, true).await, old);
        tokio::time::timeout(Duration::from_secs(3), f.http.entered.notified())
            .await
            .unwrap();
        let forced = {
            let catalog = Arc::clone(&f.catalog);
            tokio::spawn(async move { catalog.fetch(true, true).await })
        };
        f.http.release.as_ref().unwrap().notify_one();
        tokio::time::timeout(Duration::from_secs(3), f.http.entered.notified())
            .await
            .unwrap();
        assert_eq!(*f.catalog.snapshot(), old);
        assert!(f.catalog.state.lock().unwrap().retry_after > now());
        *f.http.response.lock().unwrap() =
            (StatusCode::OK, "{\"new\":{}}".into(), "new-etag".into());
        f.http.release.as_ref().unwrap().notify_one();
        let data = tokio::time::timeout(Duration::from_secs(3), forced)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*data, json!({"new": {}}));
        assert_eq!(f.catalog.state.lock().unwrap().retry_after, 0.0);
        assert_eq!(
            std::fs::read_to_string(f.catalog.etag_path()).unwrap(),
            "new-etag"
        );
        assert_eq!(f.http.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn invalid_network_payload_keeps_previous_registry() {
        let f = fixture(false).await;
        let old = f.catalog.fetch(false, true).await;
        for body in ["broken", "{}", "[]", "null"] {
            f.http.response.lock().unwrap().1 = body.into();
            assert_eq!(f.catalog.fetch(true, true).await, old);
            assert!(f.catalog.state.lock().unwrap().retry_after > now());
            assert_eq!(
                serde_json::from_slice::<Value>(&std::fs::read(f.catalog.cache_path()).unwrap())
                    .unwrap(),
                *old
            );
        }
    }
    #[test]
    fn metadata_and_overrides_match_python() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/cloud-metadata-goldens.json"))
                .unwrap();
        for (index, case) in cases.iter().enumerate() {
            let result = resolve_capabilities(
                &case["registry"],
                &case["config"],
                case["provider"].as_str().unwrap(),
                case["model"].as_str().unwrap(),
            );
            assert_eq!(
                serde_json::to_value(result).unwrap(),
                case["expected"],
                "case {index}"
            );
            assert_eq!(
                json!(resolve_context(
                    &case["registry"],
                    &case["config"],
                    case["provider"].as_str().unwrap(),
                    case["model"].as_str().unwrap()
                )),
                case["context"],
                "context case {index}"
            );
        }
    }

    #[tokio::test]
    async fn vision_catalog_stage_uses_real_http_and_live_override_input() {
        let f = fixture(false).await;
        // The registry endpoint is queried using the static Hermes provider map.
        f.http.response.lock().unwrap().1 = json!({"google": {"models": {"image-model": {"modalities": {"input": ["image"]}, "tool_call": true}}}}).to_string();
        assert_eq!(
            crate::image_routing::lookup_catalog_vision(
                "gemini",
                "image-model",
                &json!({}),
                &f.catalog
            )
            .await,
            Some(true)
        );
        let cfg =
            json!({"model_overrides": {"google": {"image-model": {"supports_vision": false}}}});
        assert_eq!(
            crate::image_routing::lookup_catalog_vision("gemini", "image-model", &cfg, &f.catalog)
                .await,
            Some(false)
        );
        assert_eq!(f.http.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_provider_override_never_fetches_registry() {
        let f = fixture(false).await;
        let config = json!({"model_overrides": {"local": {"_default": {"supports_vision": true}}}});
        assert!(
            f.catalog
                .capabilities("local", "image", &config, true)
                .await
                .unwrap()
                .supports_vision
        );
        assert!(f.http.requests.lock().unwrap().is_empty());
        assert!(!f.catalog.cache_path().exists());
        assert!(f
            .catalog
            .capabilities("openai", "image", &json!({}), false)
            .await
            .is_none());
        assert!(f.http.requests.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn explicit_context_precedes_network_and_gap_default_does_not_mask_catalog() {
        let f = fixture(false).await;
        let config = json!({"model_overrides": {"openai": {"m": {"context_window": 1234}, "_default": {"context_window": 500}}}});
        assert_eq!(
            f.catalog.context_window("openai", "m", &config, true).await,
            Some(1234)
        );
        assert!(f.http.requests.lock().unwrap().is_empty());
        f.http.response.lock().unwrap().1 = json!({"openai": {"models": {"other": {"limit": {"context": 9999}}, "missing-size": {}}}}).to_string();
        assert_eq!(
            f.catalog
                .context_window("openai", "other", &config, true)
                .await,
            Some(9999)
        );
        assert_eq!(
            f.catalog
                .context_window("openai", "missing-size", &config, false)
                .await,
            Some(500)
        );
        assert_eq!(f.http.requests.lock().unwrap().len(), 1);
    }
}
