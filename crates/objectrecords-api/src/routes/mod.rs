//! HTTP route handlers for the api layer.
//!
//! Phase 4.0 ships two endpoints (decision Sub-Q2 = (I) minimum):
//!
//! - [`health`] — GET /health for readiness checks.
//! - [`records`] — GET /records/:id for record retrieval.
//!
//! Phase 4.1+ extends the surface (write endpoints, list, transition,
//! creo-memories integration receiver, etc.). Each module exposes a
//! `router()` constructor returning an [`axum::Router`] specific to
//! its endpoint group; [`crate::build_router`] composes them.

pub mod assets;
pub mod health;
pub mod records;
