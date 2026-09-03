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

/// Shared runtime state for the gateway. Grows as subsystems are ported.
#[derive(Clone)]
pub struct AppState {
    ready: Arc<AtomicBool>,
    pub agent: Arc<dyn AgentClient>,
}

impl AppState {
    pub fn new(agent: Arc<dyn AgentClient>) -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(false)),
            agent,
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
    if state.is_ready() {
        (StatusCode::OK, Json(json!({ "ready": true })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ready": false })),
        )
    }
}
