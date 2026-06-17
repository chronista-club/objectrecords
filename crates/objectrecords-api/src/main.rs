//! `objectrecords-api` binary entrypoint.
//!
//! Reads the [`objectrecords_db::ObjectRecordsDbConfig`] from
//! `OBJECTRECORDS_SURREAL_*` env vars (same idiom as the db crate's
//! integration tests), connects to SurrealDB, applies the schema,
//! and serves HTTP on `${OBJECTRECORDS_API_BIND}` (default
//! `0.0.0.0:8000`).
//!
//! Phase 4.E — Auth0 enforcement is resolved from `OBJECTRECORDS_AUTH0_*`
//! env vars via [`auth_env_from_vars`]: all three set = live (JWT
//! enforced, JWKS fetched at startup), all absent = dev (no-auth public
//! per internal design notes). A partial set aborts startup.

use std::env;

use objectrecords_api::{
    AppState, Auth0Verifier, AuthEnv, StorageEnv, auth_env_from_vars, build_router,
    storage_env_from_vars,
};
use objectrecords_db::{ObjectRecordsDb, ObjectRecordsDbConfig, SurrealRecordRepository};
use objectrecords_storage::s3_compat::{S3CompatConfig, S3CompatStorage};
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,objectrecords_api=debug")),
        )
        .init();

    let config = ObjectRecordsDbConfig {
        endpoint: env::var("OBJECTRECORDS_SURREAL_ENDPOINT")
            .unwrap_or_else(|_| "ws://127.0.0.1:12000".to_string()),
        namespace: env::var("OBJECTRECORDS_SURREAL_NAMESPACE")
            .unwrap_or_else(|_| "objectrecords".to_string()),
        database: env::var("OBJECTRECORDS_SURREAL_DATABASE")
            .unwrap_or_else(|_| "main".to_string()),
        username: env::var("OBJECTRECORDS_SURREAL_USERNAME")
            .unwrap_or_else(|_| "admin".to_string()),
        password: env::var("OBJECTRECORDS_SURREAL_PASSWORD")
            .unwrap_or_else(|_| "admin-local-dev".to_string()),
    };
    let bind = env::var("OBJECTRECORDS_API_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8000".to_string());

    tracing::info!(
        endpoint = %config.endpoint,
        namespace = %config.namespace,
        database = %config.database,
        "connecting to SurrealDB",
    );
    let db = ObjectRecordsDb::connect(config).await?;
    let repo = SurrealRecordRepository::new(db);

    // Resolve Auth0 enforcement from the environment. Empty/whitespace
    // values are normalised to "unset" so a blank line in an env file
    // does not count as a partial config.
    let auth_env = auth_env_from_vars(|key| {
        env::var(key).ok().filter(|value| !value.trim().is_empty())
    })?;
    let mut state = match auth_env {
        AuthEnv::Disabled => {
            tracing::warn!(
                "auth DISABLED — no-auth public mode (dev stage); set \
                 OBJECTRECORDS_AUTH0_AUDIENCE / _ISSUER / _JWKS_URL to enforce JWT",
            );
            AppState::new(repo)
        }
        AuthEnv::Enabled {
            audience,
            issuer,
            jwks_url,
        } => {
            tracing::info!(%audience, %issuer, %jwks_url, "auth ENABLED — fetching JWKS");
            // Fail-closed: if the JWKS cannot be fetched the server must
            // not start, because every request would fail verification.
            let jwks_json = reqwest::get(&jwks_url)
                .await?
                .error_for_status()?
                .text()
                .await?;
            let verifier = Auth0Verifier::from_jwks_json(&jwks_json, audience, issuer)?;
            AppState::with_auth(repo, verifier)
        }
    };

    // Phase 4.F — storage backend. Same all-or-nothing discipline as
    // Auth0: 4 required vars + 2 optional (region / allow_http). Empty
    // strings are normalised to unset, partial config aborts startup.
    let storage_env = storage_env_from_vars(|key| {
        env::var(key).ok().filter(|value| !value.trim().is_empty())
    })?;
    match storage_env {
        StorageEnv::Disabled => {
            tracing::warn!(
                "storage DISABLED — POST /assets + BlobRef fix() will surface \
                 500 Internal; set OBJECTRECORDS_S3_ENDPOINT / _BUCKET / \
                 _ACCESS_KEY / _SECRET_KEY to enable",
            );
        }
        StorageEnv::Enabled {
            endpoint,
            bucket,
            region,
            access_key,
            secret_key,
            allow_http,
        } => {
            tracing::info!(%endpoint, %bucket, %region, allow_http,
                "storage ENABLED — building S3CompatStorage");
            let storage = S3CompatStorage::from_config(S3CompatConfig {
                endpoint,
                region,
                bucket,
                access_key,
                secret_key,
                allow_http,
            })?;
            state = state.with_storage(storage);
        }
    }

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "objectrecords-api listening");
    axum::serve(listener, app).await?;
    Ok(())
}
