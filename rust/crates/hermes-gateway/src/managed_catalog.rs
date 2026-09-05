//! Packaged managed-runtime catalog and best-effort in-memory refresh.
//! Ports the loading/refresh boundary of local_runtime/catalog.py.
//! Model selection and hardware estimation remain separate consumers.
#![allow(dead_code)]

use crate::python_value::{integer, numeric_text, truthy};
use serde_json::{json, Map, Value};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

const CATALOG_URL: &str = "https://raw.githubusercontent.com/NousResearch/hermes-agent/main/hermes_cli/local_runtime/catalog.json";
const REFRESH_TTL: Duration = Duration::from_secs(6 * 3600);

pub struct ManagedCatalog {
    document: RwLock<Arc<Value>>,
    attempted: Mutex<Option<Instant>>,
    url: String,
    client: Option<reqwest::Client>,
}

impl ManagedCatalog {
    pub fn packaged() -> Arc<Self> {
        static PACKAGED: LazyLock<Arc<ManagedCatalog>> = LazyLock::new(|| {
            let raw: Value =
                serde_json::from_str(include_str!("../../../tools/managed-catalog.json"))
                    .expect("valid packaged managed catalog JSON");
            let document =
                normalize_catalog(&raw).expect("supported packaged managed catalog schema");
            ManagedCatalog::with_document(document)
        });
        Arc::clone(&PACKAGED)
    }

    /// For callers which already own a loaded catalog, including source fixtures.
    pub fn with_document(document: Value) -> Arc<Self> {
        Arc::new(Self {
            document: RwLock::new(Arc::new(document)),
            attempted: Mutex::new(None),
            url: CATALOG_URL.into(),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .ok(),
        })
    }

    /// Readers retain an immutable snapshot across refresh without copying the catalog.
    pub fn snapshot(&self) -> Arc<Value> {
        self.document.read().unwrap().clone()
    }

    /// Reserve the attempt before I/O. Failure also consumes the TTL; force
    /// bypasses it. A successful fetch returns true even if data is unchanged.
    pub async fn refresh(&self, force: bool) -> bool {
        {
            let mut attempted = self.attempted.lock().unwrap();
            if !force && attempted.is_some_and(|at| at.elapsed() < REFRESH_TTL) {
                return false;
            }
            *attempted = Some(Instant::now());
        }
        let Some(client) = &self.client else {
            return false;
        };
        let fetched = async {
            let response = client
                .get(&self.url)
                .header("user-agent", "hermes-local-runtime")
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            normalize_catalog(&response.json::<Value>().await.ok()?)
        }
        .await;
        let Some(document) = fetched else {
            return false;
        };
        *self.document.write().unwrap() = Arc::new(document);
        true
    }

    /// The current request uses its existing snapshot. The worker repeats the
    /// TTL check so racing background schedules do not duplicate network work.
    pub fn refresh_soon(self: &Arc<Self>) {
        if self
            .attempted
            .lock()
            .unwrap()
            .is_some_and(|at| at.elapsed() < REFRESH_TTL)
        {
            return;
        }
        let catalog = Arc::clone(self);
        tokio::spawn(async move {
            catalog.refresh(false).await;
        });
    }
}

fn dictionary(value: &Value) -> Option<Value> {
    if value.is_object() {
        return Some(value.clone());
    }
    let mut result = Map::new();
    for pair in value.as_array()? {
        let items = match pair {
            Value::Array(items) => items.clone(),
            Value::String(text) => text.chars().map(|c| json!(c.to_string())).collect(),
            _ => return None,
        };
        if items.len() != 2 {
            return None;
        }
        result.insert(items[0].as_str()?.into(), items[1].clone());
    }
    Some(Value::Object(result))
}

fn asset(value: &Value) -> Option<Value> {
    if !truthy(value) {
        return Some(Value::Null);
    }
    Some(json!({
        "path": value.get("path")?,
        "size_bytes": integer(value.get("size_bytes")?)?,
        "local": value.get("local").unwrap_or(&Value::Null),
    }))
}

/// Match the dataclass constructor projection: retain known fields, apply
/// defaults/coercions, and reject a malformed document before replacing state.
fn normalize_catalog(document: &Value) -> Option<Value> {
    if integer(document.get("schema_version").unwrap_or(&Value::Null))? != json!(1) {
        return None;
    }
    let mut models = Vec::new();
    for source in document.get("models")?.as_array()? {
        let mut model = Map::new();
        for field in ["id", "display_name", "description", "repo"] {
            model.insert(field.into(), source.get(field)?.clone());
        }
        for field in [
            "n_ctx_train",
            "full_layers",
            "recurrent_layers",
            "per_layer_f16",
        ] {
            model.insert(field.into(), integer(source.get(field)?)?);
        }
        for (field, default) in [
            ("swa_layers", 0),
            ("swa_window", 0),
            ("mtp_draft_depth", 3),
            ("n_vocab", 0),
            ("quality", 0),
        ] {
            model.insert(
                field.into(),
                integer(source.get(field).unwrap_or(&json!(default)))?,
            );
        }
        for field in ["moe", "mtp"] {
            model.insert(
                field.into(),
                json!(truthy(source.get(field).unwrap_or(&Value::Null))),
            );
        }
        for field in ["mmproj", "draft"] {
            model.insert(
                field.into(),
                asset(source.get(field).unwrap_or(&Value::Null))?,
            );
        }
        let engine = match source.get("min_engine") {
            None => String::new(),
            Some(Value::String(value)) => value.clone(),
            Some(value) => crate::python_value::python_repr(value),
        };
        model.insert("min_engine".into(), Value::String(engine));
        let fraction = match source.get("decode_fraction") {
            None => 1.0,
            Some(Value::Bool(value)) => {
                if *value {
                    1.0
                } else {
                    0.0
                }
            }
            Some(Value::String(value)) => numeric_text(value)?.parse().ok()?,
            Some(value) => value.as_f64()?,
        };
        model.insert(
            "decode_fraction".into(),
            serde_json::Number::from_f64(fraction)?.into(),
        );
        model.insert(
            "sampling".into(),
            dictionary(source.get("sampling").unwrap_or(&json!({})))?,
        );
        if !model["sampling"].is_object() {
            return None;
        }
        let mut variants = Vec::new();
        for variant in source.get("variants")?.as_array()? {
            let files: Option<Vec<Value>> = variant
                .get("files")?
                .as_array()?
                .iter()
                .map(asset)
                .collect();
            variants.push(json!({"quant": variant.get("quant")?, "files": files?,
                "validated": truthy(variant.get("validated").unwrap_or(&Value::Null))}));
        }
        model.insert("variants".into(), Value::Array(variants));
        models.push(Value::Object(model));
    }
    Some(json!({"schema_version": 1, "models": models}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::get,
        Json, Router,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[test]
    fn catalog_loading_matches_python() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../tools/managed-catalog-goldens.json"))
                .unwrap();
        for (index, case) in cases.iter().enumerate() {
            assert_eq!(
                normalize_catalog(&case["input"]).unwrap_or(Value::Null),
                case["expected"],
                "case {index}"
            );
        }
        assert_eq!(*ManagedCatalog::packaged().snapshot(), cases[0]["expected"]);
    }

    #[derive(Clone)]
    struct ServerState {
        response: Arc<Mutex<(StatusCode, Value)>>,
        calls: Arc<AtomicUsize>,
        entered: Arc<Notify>,
        release: Option<Arc<Notify>>,
    }

    async fn serve(
        State(state): State<ServerState>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(headers["user-agent"], "hermes-local-runtime");
        state.calls.fetch_add(1, Ordering::SeqCst);
        state.entered.notify_one();
        if let Some(release) = &state.release {
            release.notified().await;
        }
        let (status, value) = state.response.lock().unwrap().clone();
        (status, Json(value))
    }

    async fn fixture(
        block: bool,
    ) -> (
        Arc<ManagedCatalog>,
        ServerState,
        tokio::task::JoinHandle<()>,
    ) {
        let state = ServerState {
            response: Arc::new(Mutex::new((
                StatusCode::OK,
                json!({"schema_version": 1, "models": []}),
            ))),
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(Notify::new()),
            release: block.then(|| Arc::new(Notify::new())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut catalog =
            ManagedCatalog::with_document((*ManagedCatalog::packaged().snapshot()).clone());
        Arc::get_mut(&mut catalog).unwrap().url =
            format!("http://{}/catalog", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/catalog", get(serve))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (catalog, state, task)
    }

    #[tokio::test]
    async fn refresh_swaps_valid_catalog_and_throttles_attempts() {
        let (catalog, state, server) = fixture(false).await;
        assert!(catalog.refresh(false).await);
        assert_eq!(catalog.snapshot()["models"], json!([]));
        assert!(!catalog.refresh(false).await);
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        // Identical data still means the forced fetch succeeded.
        assert!(catalog.refresh(true).await);
        *catalog.attempted.lock().unwrap() = Some(Instant::now() - REFRESH_TTL);
        assert!(catalog.refresh(false).await);
        assert_eq!(state.calls.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn failed_refresh_keeps_previous_catalog_and_consumes_ttl() {
        let (catalog, state, server) = fixture(false).await;
        let original = catalog.snapshot();
        for response in [
            (StatusCode::BAD_GATEWAY, json!({})),
            (StatusCode::OK, json!({"schema_version": 2, "models": []})),
            (StatusCode::OK, json!({"schema_version": 1, "models": [{}]})),
        ] {
            *state.response.lock().unwrap() = response;
            assert!(!catalog.refresh(true).await);
            let count = state.calls.load(Ordering::SeqCst);
            assert!(!catalog.refresh(false).await);
            assert_eq!(state.calls.load(Ordering::SeqCst), count);
            assert_eq!(catalog.snapshot(), original);
        }
        server.abort();
    }

    #[tokio::test]
    async fn background_refresh_serves_old_snapshot_during_io() {
        let (catalog, state, server) = fixture(true).await;
        let original = catalog.snapshot();
        catalog.refresh_soon();
        tokio::time::timeout(Duration::from_secs(2), state.entered.notified())
            .await
            .unwrap();
        assert_eq!(catalog.snapshot(), original);
        catalog.refresh_soon();
        assert!(!catalog.refresh(false).await);
        state.release.as_ref().unwrap().notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while catalog.snapshot() == original {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(catalog.snapshot()["models"], json!([]));
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
