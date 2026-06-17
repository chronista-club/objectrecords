//! `POST /assets` — binary asset upload (Phase 4.F).
//!
//! Stage 3 / Phase 4.F (decision spark `docs/phase-4f-write-endpoints.md`).
//!
//! Wire shape:
//!
//! ```text
//! POST /assets
//! Authorization: Bearer <jwt>      # scope: objectrecords:write
//! Content-Type: <any>              # not parsed by server (bytes are bytes)
//! Body: <raw binary>               # bound by ASSET_BODY_LIMIT
//!
//! 201 Created (newly stored)
//!   {"key":"/assets/<sha256>","size":N,"sha256":"<hex>","outcome":"wrote"}
//! 200 OK      (dedup — server-side `If-None-Match: *` collision)
//!   {"key":"/assets/<sha256>","size":N,"sha256":"<hex>","outcome":"already_exists"}
//! ```
//!
//! Pipeline:
//! 1. **JWT** — `verify_jwt_middleware` (outer route_layer) attaches `Claims`.
//! 2. **Scope** — `require_scope_write` checks `objectrecords:write`.
//! 3. **Validate** — [`validate_asset_body`] (pure fn, unit-tested) enforces
//!    non-empty + size limit and derives the content-addressed key.
//! 4. **Store** — [`objectrecords_storage::BlobStorage::put_if_absent`]
//!    writes the bytes under `/assets/<sha256>` with S3 `If-None-Match: *`
//!    semantics (decision #22) for race-free dedup.
//!
//! Size limit:
//! - Default: 10 MiB ([`ASSET_BODY_LIMIT`]).
//! - axum's `DefaultBodyLimit` is overridden per-route via [`router`].
//! - Larger uploads should use the presigned-PUT flow (deferred — Phase 4.F.2).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::post;
use objectrecords_storage::{BlobStorage, PutOutcome};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::error;

use crate::AppState;
use crate::error::ApiError;

/// Default body limit for `POST /assets`: 10 MiB.
///
/// Justification: creo-memories attachments are typically images /
/// docs in the < 5 MiB range; 10 MiB leaves ample headroom while
/// keeping the in-memory buffer per request bounded. Larger uploads
/// (video, full-res assets) should route through the presigned-PUT
/// flow which the api crate exposes via
/// [`objectrecords_storage::s3_compat::S3CompatStorage::presign_put`]
/// (Phase 4.F.2).
pub const ASSET_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// Response body for [`POST /assets`](crate::routes::assets).
///
/// The `outcome` discriminator lets callers (creo-web) distinguish
/// 「新規に格納された」 (`"wrote"`) and 「既に同 sha256 があった」
/// (`"already_exists"`) without re-deriving from status alone.
#[derive(Debug, Serialize)]
pub struct AssetUploadResponse {
    /// Storage key — content-addressed under `/assets/<sha256>`.
    /// Stable across reboots, idempotent under retries.
    pub key: String,
    /// Number of bytes the server accepted.
    pub size: u64,
    /// Hex-encoded SHA-256 of the body. Lower-case, no prefix.
    pub sha256: String,
    /// `"wrote"` if newly stored, `"already_exists"` if dedup'd.
    pub outcome: &'static str,
}

/// Validates the uploaded body and derives its content-addressed key.
///
/// Extracted out of the handler so it can be unit-tested without
/// touching axum extractors or the storage backend.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] for an empty body — there is no
///   meaningful `Record<Mutable>` whose body is zero bytes; rejecting
///   at the api layer keeps storage clean of degenerate keys.
/// - [`ApiError::PayloadTooLarge`] when the body exceeds `limit`.
pub(crate) fn validate_asset_body(body: &Bytes, limit: usize) -> Result<(String, String), ApiError> {
    if body.is_empty() {
        return Err(ApiError::BadRequest(
            "asset body must not be empty".to_string(),
        ));
    }
    if body.len() > limit {
        return Err(ApiError::PayloadTooLarge {
            actual: body.len(),
            max: limit,
        });
    }
    let hex = format!("{:x}", Sha256::digest(body));
    let key = format!("/assets/{hex}");
    Ok((key, hex))
}

async fn post_one(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<(StatusCode, Json<AssetUploadResponse>), ApiError> {
    let storage = ensure_storage(&state)?;
    let (key, hex) = validate_asset_body(&body, ASSET_BODY_LIMIT)?;
    let outcome = storage.put_if_absent(&key, body.clone()).await.map_err(|err| {
        error!(?err, %key, "storage.put_if_absent failed for asset upload");
        ApiError::Internal
    })?;
    let status = match outcome {
        PutOutcome::Wrote => StatusCode::CREATED,
        PutOutcome::AlreadyExists => StatusCode::OK,
    };
    let resp = AssetUploadResponse {
        key,
        size: body.len() as u64,
        sha256: hex,
        outcome: match outcome {
            PutOutcome::Wrote => "wrote",
            PutOutcome::AlreadyExists => "already_exists",
        },
    };
    Ok((status, Json(resp)))
}

/// Fetches the storage handle from the [`AppState`] or returns
/// [`ApiError::Internal`] (with a log line) if the deploy left it
/// unconfigured. Phase 4.F: dev / scratch may run without storage
/// (the route is still mounted but errors out on attempted upload —
/// `503 Service Unavailable` semantically; the API layer surfaces it
/// as `500 Internal` so callers see a single retryable status, and
/// the log records the misconfiguration).
fn ensure_storage(state: &AppState) -> Result<Arc<objectrecords_storage::s3_compat::S3CompatStorage>, ApiError> {
    state.storage.clone().ok_or_else(|| {
        error!(
            "POST /assets invoked but OBJECTRECORDS_S3_* env vars are unset; \
             storage backend not configured"
        );
        ApiError::Internal
    })
}

/// Returns the `Router` fragment carrying [`POST /assets`].
///
/// The router applies a per-route body limit override
/// ([`DefaultBodyLimit::max`]) so the global axum default does not
/// silently cap uploads below [`ASSET_BODY_LIMIT`]. JWT + scope
/// gating is layered at [`crate::build_router`].
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/assets", post(post_one))
        .layer(DefaultBodyLimit::max(ASSET_BODY_LIMIT))
}

#[cfg(test)]
mod tests {
    //! Small-tier unit tests (Phase 4.F / Stage 3).
    //!
    //! End-to-end coverage (multipart-less raw body roundtrip, dedup
    //! against a live SeaweedFS) lives in the env-gated Medium tier
    //! integration suite (`tests/http_smoke.rs` once the S3 backend
    //! is wired).

    use super::*;

    #[test]
    fn empty_body_is_bad_request() {
        let body = Bytes::from_static(b"");
        let err = validate_asset_body(&body, ASSET_BODY_LIMIT)
            .expect_err("empty body must be rejected");
        match err {
            ApiError::BadRequest(msg) => {
                assert!(msg.contains("empty"), "message should mention empty: {msg}");
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn over_limit_is_payload_too_large() {
        let body = Bytes::from(vec![0xCDu8; 1024]);
        let err = validate_asset_body(&body, 100).expect_err("over-limit must be rejected");
        match err {
            ApiError::PayloadTooLarge { actual, max } => {
                assert_eq!(actual, 1024);
                assert_eq!(max, 100);
            }
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn key_uses_sha256_hex_lowercase() {
        // SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        // (RFC 6234 test vector + sha2 crate default)
        let body = Bytes::from_static(b"hello");
        let (key, hex) = validate_asset_body(&body, ASSET_BODY_LIMIT).expect("hello must be valid");
        let expected_hex = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(hex, expected_hex);
        assert_eq!(key, format!("/assets/{expected_hex}"));
        // Sanity-check the hex string is lowercase + has no separator.
        assert_eq!(hex, hex.to_lowercase());
        assert!(!hex.contains(':'));
        assert!(!hex.contains('-'));
    }

    #[test]
    fn at_exact_limit_passes() {
        // Boundary test — `body.len() > limit` (strict gt), so equal length
        // must be accepted. Catches off-by-one bugs in the limit check.
        let body = Bytes::from(vec![0xABu8; 1024]);
        let (_key, _hex) = validate_asset_body(&body, 1024).expect("at-limit body must pass");
    }

    #[test]
    fn deterministic_key_across_calls() {
        // Same bytes must always yield the same key — guarantees dedup
        // semantics work even when callers retry across processes.
        let body = Bytes::from_static(b"deterministic-payload");
        let (k1, h1) = validate_asset_body(&body, ASSET_BODY_LIMIT).unwrap();
        let (k2, h2) = validate_asset_body(&body, ASSET_BODY_LIMIT).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(h1, h2);
    }
}
