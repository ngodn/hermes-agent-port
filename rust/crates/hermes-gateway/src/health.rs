//! Shared app state, health and readiness endpoints.
//!
//! Health mirrors `gateway/readiness.py` / `gateway/status.py`: `/healthz` is a
//! liveness check (process is up), `/readyz` reports whether the gateway has
//! finished startup and can accept traffic. [`AppState`] is the shared handle
//! every route gets; it also carries the [`AgentClient`] used to run turns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::json;

use crate::agent::AgentClient;
use crate::{config_file, readiness};

/// Shared runtime state for the gateway. Grows as subsystems are ported.
#[derive(Clone)]
pub struct AppState {
    ready: Arc<AtomicBool>,
    pub agent: Arc<dyn AgentClient>,
    /// Parsed user config (`$HERMES_HOME/config.yaml`) as a JSON value, the
    /// shape the ported resolvers consume. Empty object when absent.
    pub user_config: Arc<serde_json::Value>,
    /// Configured model, if any, for the readiness `model` probe.
    pub configured_model: Option<String>,
}

impl AppState {
    pub fn new(
        agent: Arc<dyn AgentClient>,
        user_config: Arc<serde_json::Value>,
        configured_model: Option<String>,
    ) -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(false)),
            agent,
            user_config,
            configured_model,
        }
    }

    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
}

pub async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let started = state.is_ready();
    // Bounded, non-destructive probes (config / model / state_db). Run on a
    // blocking thread since they touch the filesystem and SQLite.
    let model = state.configured_model.clone();
    let report = tokio::task::spawn_blocking(move || {
        readiness::collect_readiness(&config_file::hermes_home(), model.as_deref())
    })
    .await
    .ok();

    // Ready requires startup complete AND no degraded probe.
    let probes_ok = report.as_ref().map(|r| r.status == "ok").unwrap_or(false);
    let ready = started && probes_ok;
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(json!({
            "ready": ready,
            "started": started,
            "readiness": report,
        })),
    )
}
