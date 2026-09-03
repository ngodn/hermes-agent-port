//! Health and readiness endpoints.
//!
//! Mirrors the intent of `gateway/readiness.py` / `gateway/status.py`:
//! `/healthz` is a liveness check (process is up), `/readyz` reports whether
//! the gateway has finished startup and can accept traffic.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::json;

/// Shared runtime state for the gateway. Grows as platform adapters are ported.
#[derive(Clone, Default)]
pub struct AppState {
    ready: Arc<AtomicBool>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
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
    if state.is_ready() {
        (StatusCode::OK, Json(json!({ "ready": true })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ready": false })),
        )
    }
}
