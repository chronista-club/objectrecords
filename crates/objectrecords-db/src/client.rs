//! [`ObjectRecordsDb`] — connection wrapper for the Object Records
//! SurrealDB instance.
//!
//! Phase 3 design (decision #28, axis Sub-Q1 = β): this crate depends
//! on `surrealdb` directly rather than going through
//! `creo-memories`'s `DbClient`. The decoupling preserves stack
//! symmetry — `objectrecords-storage` wraps `object_store` directly
//! by the same logic — and keeps release cycles independent.
//!
//! The [`ObjectRecordsDb`] type wraps the SDK's
//! [`Surreal<Any>`][surrealdb::Surreal] in an [`Arc`] so the
//! repository (Phase 3.1) and any future api-layer integration
//! (Phase 4+) can share a single live connection without re-doing the
//! WebSocket handshake. The wrapper is `Clone` (cheap, `Arc::clone`)
//! and `Send + Sync` (via the SDK).

use std::sync::Arc;

use surrealdb::Surreal;
use surrealdb::engine::any::{self, Any};
use surrealdb::opt::auth::Root;

use crate::error::DbError;
use crate::schema;

/// Connection configuration for [`ObjectRecordsDb::connect`].
///
/// All fields are mandatory; tests construct them via the helpers in
/// `tests/surrealdb_local.rs`, production callers will populate them
/// from env vars (see Phase 3.1 Sub-Q4 default — `OBJECTRECORDS_SURREAL_*`)
/// resolved through 1Password (Phase 4+ will add the resolver).
#[derive(Debug, Clone)]
pub struct ObjectRecordsDbConfig {
    /// Endpoint URL — e.g. `ws://127.0.0.1:12000` for the local
    /// `surreal-local` dev container, or `wss://...` for production.
    /// `any::connect` handles scheme dispatch.
    pub endpoint: String,
    /// SurrealDB namespace. Production uses `objectrecords`;
    /// integration tests use `objectrecords_test` (decision #25, axis
    /// A=(i): namespace-isolated, instance-shared with creo-memories).
    pub namespace: String,
    /// SurrealDB database within the namespace. Phase 3.1 uses a
    /// single `main` database; Phase 4+ may carve out `staging` etc.
    pub database: String,
    /// Root-level username for `signin`. Dev uses `admin`; production
    /// reads from the deployment secrets vault.
    pub username: String,
    /// Root-level password for `signin`.
    pub password: String,
}

/// Live SurrealDB connection scoped to the configured ns/db, with the
/// Object Records schema applied.
///
/// Acquired via [`Self::connect`]; clones share the underlying SDK
/// handle (SDK is internally [`Arc`]-based, the explicit wrap matches
/// `S3CompatStorage`'s `Arc<AmazonS3>` idiom — uniform stack symmetry).
#[derive(Debug, Clone)]
pub struct ObjectRecordsDb {
    inner: Arc<Surreal<Any>>,
}

impl ObjectRecordsDb {
    /// Opens a connection, signs in, scopes to ns/db, and applies the
    /// schema (idempotent — `OVERWRITE`, see [`schema::apply`]).
    ///
    /// The order matters: `signin` must precede `use_ns` (the chosen
    /// ns/db requires Root-level auth to switch into), and
    /// [`schema::apply`] must come after `use_ns` (otherwise the
    /// `DEFINE TABLE` statements have no scope).
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Surreal`] on any of: WS handshake failure,
    /// invalid credentials, missing/forbidden namespace, schema
    /// statement rejection.
    pub async fn connect(config: ObjectRecordsDbConfig) -> Result<Self, DbError> {
        let db: Surreal<Any> = any::connect(&config.endpoint).await?;
        db.signin(Root {
            username: config.username.clone(),
            password: config.password.clone(),
        })
        .await?;
        db.use_ns(&config.namespace)
            .use_db(&config.database)
            .await?;
        schema::apply(&db).await?;
        Ok(Self {
            inner: Arc::new(db),
        })
    }

    /// Returns a borrowed handle to the underlying SDK connection, for
    /// the repository layer to issue SurrealQL.
    #[must_use]
    pub fn inner(&self) -> &Surreal<Any> {
        &self.inner
    }
}
