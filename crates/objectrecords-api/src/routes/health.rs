//! `GET /health` — lightweight readiness probe.
//!
//! Phase 4.0 narrow scope: returns a fixed `{"status":"ok"}` body
//! without consulting downstream services. Phase 4.x can extend
//! into a deep health check (DB connectivity, schema apply, etc.)
//! gated behind a `?deep=true` query param without breaking the
//! existing shape.

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde::Serialize;

use crate::AppState;

/// Wire form of the health response body.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Always the literal `"ok"` in Phase 4.0; Phase 4.x may switch
    /// to an enum-like literal set (`"degraded"`, `"down"`, ...).
    pub status: &'static str,
}

async fn handler() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// Returns the `Router` fragment for the health endpoints.
pub fn router() -> Router<AppState> {
    Router::new().route("/health", get(handler))
}
