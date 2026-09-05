//! Managed local vision capabilities from staged files, server state, and catalog.
//! Ports capabilities.py and its immediate bootstrap/endpoint dependencies.
//! Callers supply the shared Hermes root and can use the packaged catalog.
//! Catalog refresh is explicit; supervisor boot/lifecycle remains separate.
#![allow(dead_code)]

use fancy_regex::Regex;
use serde_json::Value;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::Duration,
};

pub const ACCEPTED_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg"];

pub struct ManagedCapabilities {
    root: PathBuf,
    catalog: Arc<crate::managed_catalog::ManagedCatalog>,
    client: Option<reqwest::Client>,
}

struct Endpoint {
    base_url: String,
    api_key: String,
}

impl ManagedCapabilities {
    pub fn from_packaged(root: PathBuf) -> Self {
        Self::with_catalog(root, crate::managed_catalog::ManagedCatalog::packaged())
    }

    pub async fn refresh_catalog(&self, force: bool) -> bool {
        self.catalog.refresh(force).await
    }

    pub fn refresh_catalog_soon(&self) {
        self.catalog.refresh_soon();
    }

    pub fn new(root: PathBuf, catalog: Value) -> Self {
        Self::with_catalog(
            root,
            crate::managed_catalog::ManagedCatalog::with_document(catalog),
        )
    }

    fn with_catalog(root: PathBuf, catalog: Arc<crate::managed_catalog::ManagedCatalog>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .read_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .ok();
        Self {
            root,
            catalog,
            client,
        }
    }

    pub async fn is_managed_provider(&self, provider: &str, base_url: &str) -> bool {
        match provider
            .trim_matches(python_whitespace)
            .to_lowercase()
            .as_str()
        {
            "llamacpp" | "llama.cpp" | "llama-cpp" => true,
            "custom" if !base_url.is_empty() => self.state_endpoint().await.is_some_and(|state| {
                base_url.trim_end_matches('/') == state.base_url.trim_end_matches('/')
            }),
            _ => false,
        }
    }

    pub async fn managed_model_supports_vision(&self, model: &str) -> Option<bool> {
        if model.is_empty()
            || !staged_model_ids(&self.root.join("models")).contains(&model.to_owned())
        {
            return None;
        }
        if let Some(live) = self.props_modalities(model).await {
            return Some(live);
        }
        self.catalog_vision(model)
    }

    // A healthy port alone does not prove ownership. Keep the health request
    // even when the recorded PID is dead, matching _state_endpoint call order.
    async fn state_endpoint(&self) -> Option<Endpoint> {
        let data: Value = serde_json::from_slice(
            &std::fs::read(self.root.join("runtimes/llamacpp/server.json")).ok()?,
        )
        .ok()?;
        let base_url = data.get("base_url")?.as_str()?;
        if base_url.is_empty() {
            return None;
        }
        let pid = match data.get("pid") {
            None | Some(Value::Null) => 0,
            Some(Value::Bool(value)) => i64::from(*value),
            Some(Value::String(value)) if value.is_empty() => 0,
            Some(Value::Array(value)) if value.is_empty() => 0,
            Some(Value::Object(value)) if value.is_empty() => 0,
            Some(Value::String(value)) => value.trim_matches(python_whitespace).parse().ok()?,
            Some(Value::Number(value)) => value
                .as_i64()
                .or_else(|| value.as_f64().map(|value| value as i64))?,
            _ => return None,
        };
        let alive = pid_alive(pid);
        let health = format!("{}/health", before_last_v1(base_url));
        if invalid_request_url(&health) {
            return None;
        }
        if let Some(client) = &self.client {
            let _ = client.get(&health).send().await;
        }
        if !alive {
            return None;
        }
        Some(Endpoint {
            base_url: base_url.to_owned(),
            api_key: data.get("api_key").map(python_string).unwrap_or_default(),
        })
    }

    async fn props_modalities(&self, model: &str) -> Option<bool> {
        let state = self.state_endpoint().await?;
        // Python interpolates the model directly into the query. Do not use
        // query-pair encoding, which would change '&', '#' and space behavior.
        let url = format!("{}/props?model={model}", before_last_v1(&state.base_url));
        if invalid_request_url(&url) {
            return None;
        }
        let response = self
            .client
            .as_ref()?
            .get(url)
            .header("authorization", format!("Bearer {}", state.api_key))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let props: Value = response.json().await.ok()?;
        let modalities = props.get("modalities")?.as_object()?;
        modalities.get("vision").map(python_truthy)
    }

    fn catalog_vision(&self, model: &str) -> Option<bool> {
        let catalog = self.catalog.snapshot();
        for entry in catalog.get("models")?.as_array()? {
            for variant in entry.get("variants")?.as_array()? {
                let path = variant
                    .get("files")?
                    .as_array()?
                    .first()?
                    .get("path")?
                    .as_str()?;
                let name = posix_name(path);
                let stem = name.strip_suffix(".gguf").unwrap_or(name);
                if strip_part_suffix(stem) != model {
                    continue;
                }
                let projector = match entry.get("mmproj") {
                    None | Some(Value::Null) => return Some(false),
                    Some(projector) => projector,
                };
                let local = projector.get("local").filter(|value| python_truthy(value));
                let name = match local {
                    Some(value) => value.as_str()?,
                    None => posix_name(projector.get("path")?.as_str()?),
                };
                return Some(self.root.join("models/assets").join(name).exists());
            }
        }
        None
    }
}

fn before_last_v1(url: &str) -> &str {
    url.rsplit_once("/v1").map_or(url, |(base, _)| base)
}

fn invalid_request_url(url: &str) -> bool {
    url.bytes().any(|byte| byte <= b' ' || byte >= 127)
}

fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        value => value.to_string(),
    }
}

fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // Signal zero checks existence/permission without delivering a signal.
        unsafe {
            libc::kill(pid, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    #[cfg(not(unix))]
    {
        true
    } // Matches Python's optimistic fallback when no liveness API exists.
}

fn posix_name(path: &str) -> &str {
    path.split('/')
        .rev()
        .find(|part| !part.is_empty() && *part != ".")
        .unwrap_or("")
}

static PART_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-\p{Nd}{5}-of-\p{Nd}{5}$").unwrap());
static PART_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-(\p{Nd}{5})-of-(\p{Nd}{5})\.gguf$").unwrap());

fn strip_part_suffix(stem: &str) -> String {
    PART_SUFFIX.replace(stem, "").into_owned()
}

/// Python's glob selects names, including directories and dangling links.
/// A split counts only through its ASCII first-part name once all parts exist.
fn staged_model_ids(models: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(models) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.ends_with(".gguf").then_some(name)
        })
        .collect();
    files.sort();
    let names: HashSet<_> = files.iter().map(String::as_str).collect();
    files
        .iter()
        .filter_map(|name| {
            if let Ok(Some(parts)) = PART_FILE.captures(name) {
                if &parts[1] != "00001" {
                    return None;
                }
                let total = parts[2].chars().try_fold(0u32, |total, c| {
                    Some(total * 10 + crate::python_value::decimal_digit(c)? as u32)
                })?;
                let stem = &name[..parts.get(0)?.start()];
                if !(2..=total).all(|part| {
                    names.contains(format!("{stem}-{part:05}-of-{}.gguf", &parts[2]).as_str())
                }) {
                    return None;
                }
            }
            let stem = Path::new(name).file_stem()?.to_str()?;
            Some(strip_part_suffix(stem))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode, Uri},
        response::IntoResponse,
        Router,
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Home(PathBuf);
    impl Home {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hermes-managed-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, name: &str, bytes: &[u8]) {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        fn state(&self, url: &str, pid: u32) {
            self.write(
                "runtimes/llamacpp/server.json",
                &serde_json::to_vec(
                    &json!({"base_url": url, "api_key": "fixture-key", "pid": pid}),
                )
                .unwrap(),
            );
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn packaged_catalog_resolves_real_staged_model_and_projector() {
        let home = Home::new();
        let managed = ManagedCapabilities::from_packaged(home.0.clone());
        let catalog = managed.catalog.snapshot();
        let entry = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["mmproj"].is_object())
            .unwrap();
        let variant = &entry["variants"][0];
        for file in variant["files"].as_array().unwrap() {
            home.write(
                &format!("models/{}", posix_name(file["path"].as_str().unwrap())),
                b"",
            );
        }
        let name = posix_name(variant["files"][0]["path"].as_str().unwrap());
        let model = strip_part_suffix(name.strip_suffix(".gguf").unwrap());
        assert_eq!(
            managed.managed_model_supports_vision(&model).await,
            Some(false)
        );
        let projector = &entry["mmproj"];
        let name = projector["local"]
            .as_str()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| posix_name(projector["path"].as_str().unwrap()));
        home.write(&format!("models/assets/{name}"), b"");
        assert_eq!(
            managed.managed_model_supports_vision(&model).await,
            Some(true)
        );
    }

    #[derive(Clone)]
    struct ServerState {
        props: Value,
        health: StatusCode,
        calls: Arc<Mutex<Vec<Value>>>,
    }
    struct Server(String, tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.1.abort();
        }
    }
    async fn serve(state: ServerState) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let router = Router::new()
            .fallback(
                |State(state): State<ServerState>, uri: Uri, headers: HeaderMap| async move {
                    state.calls.lock().unwrap().push(json!([
                        uri.to_string(),
                        headers.get("authorization").and_then(|v| v.to_str().ok())
                    ]));
                    if uri.path() == "/health" {
                        (state.health, String::new()).into_response()
                    } else {
                        axum::Json(state.props).into_response()
                    }
                },
            )
            .with_state(state);
        Server(
            url,
            tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            }),
        )
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../tools/managed-capability-goldens.json"
        ))
        .unwrap()
    }

    #[test]
    fn staged_files_match_python() {
        for case in fixture()["staging"].as_array().unwrap() {
            let home = Home::new();
            for file in case["files"].as_array().unwrap() {
                let file = format!("models/{}", file.as_str().unwrap());
                if file.ends_with('/') {
                    std::fs::create_dir_all(home.0.join(file)).unwrap();
                } else {
                    home.write(&file, b"");
                }
            }
            assert_eq!(
                json!(staged_model_ids(&home.0.join("models"))),
                case["expected"],
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn real_http_and_projector_state_match_python() {
        for case in fixture()["capabilities"].as_array().unwrap() {
            let home = Home::new();
            if case["staged"] == true {
                home.write("models/a.gguf", b"");
            }
            let projector = &case["catalog"]["models"][0]["mmproj"];
            if !projector.is_null() && case["projector_present"] == true {
                let name = projector["local"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| posix_name(projector["path"].as_str().unwrap()));
                home.write(&format!("models/assets/{name}"), b"");
            }
            let props = if case["live"].is_null() {
                json!({"modalities": {}})
            } else {
                json!({"modalities": {"vision": case["live"]}})
            };
            let calls = Arc::new(Mutex::new(Vec::new()));
            let server = serve(ServerState {
                props,
                health: StatusCode::OK,
                calls: calls.clone(),
            })
            .await;
            home.state(&server.0, std::process::id());
            let managed = ManagedCapabilities::new(home.0.clone(), case["catalog"].clone());
            let runtime = crate::image_routing::InferenceRuntime {
                provider: "llamacpp",
                base_url: "",
                api_key: "",
            };
            assert_eq!(
                json!(
                    crate::image_routing::lookup_managed_vision(
                        "llamacpp",
                        "a",
                        &json!({}),
                        &runtime,
                        &managed,
                    )
                    .await
                ),
                case["expected"],
                "{case}"
            );
            let expected = if case["calls"].as_array().unwrap().is_empty() {
                json!([])
            } else {
                json!([["/health", null], ["/props?model=a", "Bearer fixture-key"]])
            };
            assert_eq!(json!(*calls.lock().unwrap()), expected, "{case}");
        }
    }

    #[tokio::test]
    async fn ownership_health_and_raw_model_query_follow_source() {
        for (pid, health) in [
            (0, StatusCode::OK),
            (std::process::id(), StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let home = Home::new();
            home.write("models/a.gguf", b"");
            let calls = Arc::new(Mutex::new(Vec::new()));
            let server = serve(ServerState {
                props: json!({"modalities": {"vision": "false"}}),
                health,
                calls: calls.clone(),
            })
            .await;
            home.state(&server.0, pid);
            let managed = ManagedCapabilities::new(home.0.clone(), json!({"models": []}));
            let expected = if pid == 0 { None } else { Some(true) };
            assert_eq!(managed.managed_model_supports_vision("a").await, expected);
            assert_eq!(calls.lock().unwrap().len(), if pid == 0 { 1 } else { 2 });
            assert!(managed.is_managed_provider(" LLAMA.CPP ", "").await);
            assert_eq!(
                managed
                    .is_managed_provider("custom", &format!("{}/", server.0))
                    .await,
                pid != 0
            );
            assert!(
                !managed
                    .is_managed_provider("custom", "http://other.invalid/v1")
                    .await
            );
            if pid != 0 {
                calls.lock().unwrap().clear();
                assert_eq!(managed.props_modalities("a&extra=1").await, Some(true));
                assert_eq!(calls.lock().unwrap()[1][0], "/props?model=a&extra=1");
                calls.lock().unwrap().clear();
                assert_eq!(managed.props_modalities("a b").await, None);
                assert_eq!(calls.lock().unwrap().len(), 1);
            }
        }
    }
}
