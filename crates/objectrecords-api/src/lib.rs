//! `objectrecords-api` — HTTP API server for Object Records.
//!
//! Phase 4.0 ships the minimum vertical slice (decision Sub-Q2 = (I)):
//! a [`build_router`] that wires `GET /health` and `GET /records/:id`
//! into a single [`axum::Router`].
//!
//! Phase 4.D Step 3 (2026-05-14) adds the optional Auth0 JWT verify
//! middleware in [`auth`]. The middleware applies only when
//! [`AppState::auth`] is `Some(_)`; dev stage runs it as `None` per
//! the 2026-05-13 no-auth simplification.
//! Phase 4.E Step 5 will lift the verifier from `Option` to required
//! and extend coverage to every endpoint.
//!
//! Phase 4 design references:
//! - **decision Sub-Q1 (axum)** — chosen for the tower-http
//!   ecosystem and for runtime alignment with the SurrealDB SDK
//!   (decision #19 / Phase 2.0 settled the `: Send` futures idiom).
//! - **decision Sub-Q4** — JSON response uses
//!   [`objectrecords_db::PersistedRow`] directly. A dedicated DTO
//!   (`RecordResponse`) is deferred until an external consumer
//!   demands a contract.
//! - **decision Sub-Q5** — error envelope is a flat
//!   `{"error":"...","kind":"..."}` body. RFC 7807 is deferred.
//! - **decision Sub-Q6** — tests use `axum::Router::oneshot` for
//!   in-process verification (`tower::ServiceExt::oneshot`), gated
//!   by the same `OBJECTRECORDS_SURREAL_TEST_*` env vars as the db
//!   crate.

#![warn(missing_docs)]

use std::sync::Arc;

use axum::Router;
use objectrecords_db::SurrealRecordRepository;
use objectrecords_storage::s3_compat::S3CompatStorage;
use tower_http::trace::TraceLayer;

pub mod auth;
pub mod error;
pub mod routes;
pub mod storage;

pub use auth::{Auth0Verifier, AuthEnv, Claims, auth_env_from_vars};
pub use error::ApiError;
pub use storage::{StorageEnv, storage_env_from_vars};

/// Application-wide state shared by every handler via axum's `State`
/// extractor. Holds the [`SurrealRecordRepository`] instance behind
/// an `Arc` (the repository is cheap to clone) plus optional
/// [`Auth0Verifier`] and storage backend handles for Phase 4.E auth
/// + Phase 4.F write endpoints.
#[derive(Clone, Debug)]
pub struct AppState {
    /// Live repository for persistence-layer access.
    pub repo: SurrealRecordRepository,
    /// JWT verifier. `None` = no-auth (dev stage public per
    /// internal design notes); `Some(_)` = enforce on routes
    /// wrapped by [`auth::verify_jwt_middleware`].
    pub auth: Option<Arc<Auth0Verifier>>,
    /// S3-compatible blob storage. `None` = the deploy never
    /// configured `OBJECTRECORDS_S3_*` env vars (write endpoints
    /// will return 503 / ApiError::Internal); `Some(_)` = ready for
    /// Phase 4.F `POST /assets` and `BlobRef` bodies.
    pub storage: Option<Arc<S3CompatStorage>>,
}

impl AppState {
    /// Constructs a no-auth, no-storage `AppState`. Used by Phase
    /// 4.D dev stage and the existing http_smoke integration tests
    /// that exercise only read endpoints.
    #[must_use]
    pub fn new(repo: SurrealRecordRepository) -> Self {
        Self {
            repo,
            auth: None,
            storage: None,
        }
    }

    /// Constructs an auth-enabled `AppState`. Used by Phase 4.E live
    /// promotion and the Phase 4.D Step 3 verify tests.
    #[must_use]
    pub fn with_auth(repo: SurrealRecordRepository, verifier: Auth0Verifier) -> Self {
        Self {
            repo,
            auth: Some(Arc::new(verifier)),
            storage: None,
        }
    }

    /// Returns a copy of `self` with `storage` attached. Builder
    /// idiom keeps the construction flow `let state =
    /// AppState::new(repo).with_storage(storage);` linear and works
    /// equally for auth-disabled (scratch) and auth-enabled (live)
    /// states.
    #[must_use]
    pub fn with_storage(mut self, storage: S3CompatStorage) -> Self {
        self.storage = Some(Arc::new(storage));
        self
    }
}

/// Composes the route fragments from each module into a single
/// [`Router`] backed by `state` and instrumented with a `tracing`
/// layer.
///
/// Routing layout:
/// - `GET /health` — open (no auth middleware), for k8s / Caddy probes.
/// - `GET /records/{id}` — wrapped by [`auth::verify_jwt_middleware`] +
///   [`auth::require_scope_read`] (`objectrecords:read`).
/// - `POST /assets` — wrapped by [`auth::verify_jwt_middleware`] +
///   [`auth::require_scope_write`] (`objectrecords:write`).
///
/// Middleware ordering note: for layered routes the **last** call to
/// `.route_layer()` is the **outermost**, so `verify_jwt_middleware`
/// is added last to run first and populate the `Claims` extension
/// that the inner scope check reads. In no-auth scratch mode the
/// verify middleware short-circuits and the scope check follows suit
/// (`state.auth.is_none()` early return), so both wrappers are
/// harmless on dev / scratch deploys.
pub fn build_router(state: AppState) -> Router {
    use axum::middleware::from_fn_with_state;

    let records = routes::records::router()
        .route_layer(from_fn_with_state(state.clone(), auth::require_scope_read))
        .route_layer(from_fn_with_state(
            state.clone(),
            auth::verify_jwt_middleware,
        ));

    let assets = routes::assets::router()
        .route_layer(from_fn_with_state(
            state.clone(),
            auth::require_scope_write,
        ))
        .route_layer(from_fn_with_state(
            state.clone(),
            auth::verify_jwt_middleware,
        ));

    Router::new()
        .merge(routes::health::router())
        .merge(records)
        .merge(assets)
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}
