//! HTTP error mapping for the api layer.
//!
//! Phase 4.0 (decision: Sub-Q5 default — simple JSON envelope, RFC
//! 7807 deferred). Each [`objectrecords_db::DbError`] is translated
//! into a corresponding [`ApiError`] variant which then renders as
//! `(StatusCode, Json<ErrorBody>)` via the [`axum::response::IntoResponse`]
//! impl. The body is intentionally minimal:
//!
//! ```json
//! { "error": "<human-readable message>", "kind": "<machine-tag>" }
//! ```
//!
//! `kind` is a stable identifier callers can switch on; `error` is a
//! best-effort prose hint that may evolve. Phase 4.x can additively
//! adopt RFC 7807 (`application/problem+json`) once an external
//! consumer asks for it.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use objectrecords_db::DbError;
use serde::Serialize;
use thiserror::Error;
use tracing::error;

/// Public-facing error type for the API. Each variant maps to a
/// specific HTTP status and a stable `kind` tag.
///
/// New variants must extend the response mapping in
/// [`ApiError::status_and_kind`] (an exhaustive match keeps the
/// compiler honest).
#[derive(Debug, Error)]
pub enum ApiError {
    /// 401 — JWT verification failed (Phase 4.D Step 3 / Phase 4.E).
    /// Per internal design notes, this is **strictly auth
    /// failure** (missing / malformed / invalid signature / wrong
    /// audience / expired). 403 (user-not-provisioned) is a separate
    /// variant landing in Phase 4.E Step 5.
    #[error("unauthorized")]
    Unauthorized,

    /// 404 — the requested record id does not exist (or has been
    /// deleted).
    #[error("record not found")]
    NotFound,

    /// 409 — `save<S>` collided with an existing record id (will
    /// surface in Phase 4.2+ once write endpoints land; included now
    /// so the mapping table is complete).
    #[error("record already exists")]
    AlreadyExists,

    /// 400 — the request was syntactically valid but violated a
    /// schema invariant (malformed UUID in path, etc.). Preserves
    /// the underlying message so callers can fix the input.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// 409 — operation rejected because the record's current state
    /// does not allow it. Phase 4.F: PUT on Fixed, fix() on Fixed,
    /// fix() with state precondition violation.
    #[error("state conflict: record is {actual}, operation expects {expected}")]
    StateConflict {
        /// State(s) the operation expects, e.g. `"mutable"` or
        /// `"mutable | snapshot"`. Kept as `&'static str` because the
        /// expected side is hard-coded per endpoint, not user input.
        expected: &'static str,
        /// Actual state of the record as observed at the db layer.
        actual: String,
    },

    /// 400 — `POST /records/:id/fix` carried a `content_hash` that
    /// does not match the digest of the trailing version's body.
    /// Phase 4.F: client must compute hash over the *exact* bytes
    /// that storage holds for the trailing version.
    #[error("content hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// Hex sha256 the server computed.
        expected: String,
        /// Hex sha256 the client submitted.
        actual: String,
    },

    /// 403 — request was authenticated but lacks required scope or
    /// (Phase 4.G+) violates owner-check. Distinct from 401
    /// `Unauthorized` (which means "no valid token"): 403 means
    /// "valid token, insufficient privilege" per
    ///.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// 413 — request body exceeds the size limit for the endpoint.
    /// Phase 4.F applies a default 10 MiB limit on `POST /assets`;
    /// larger uploads should use the presigned-PUT flow (deferred).
    #[error("payload too large: {actual} bytes (limit {max})")]
    PayloadTooLarge {
        /// Bytes the server received before refusing.
        actual: usize,
        /// Endpoint-specific maximum in bytes.
        max: usize,
    },

    /// 500 — anything that bubbles up from the storage / db layers
    /// without a more specific mapping. The underlying error is
    /// logged at `error!` level but **not** echoed in the HTTP body
    /// (avoid leaking internals).
    #[error("internal server error")]
    Internal,
}

impl ApiError {
    /// Returns the `(status, kind)` pair for the response envelope.
    fn status_and_kind(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized"),
            Self::NotFound => (StatusCode::NOT_FOUND, "NotFound"),
            Self::AlreadyExists => (StatusCode::CONFLICT, "AlreadyExists"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "BadRequest"),
            Self::StateConflict { .. } => (StatusCode::CONFLICT, "StateConflict"),
            Self::HashMismatch { .. } => (StatusCode::BAD_REQUEST, "HashMismatch"),
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, "Forbidden"),
            Self::PayloadTooLarge { .. } => (StatusCode::PAYLOAD_TOO_LARGE, "PayloadTooLarge"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "Internal"),
        }
    }
}

/// Wire form of every 4xx / 5xx response body.
#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    kind: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, kind) = self.status_and_kind();
        let body = ErrorBody {
            error: self.to_string(),
            kind,
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    //! Status + `kind` mapping unit tests (Stage 3 / Phase 4.F).
    //!
    //! Tests target the private `status_and_kind()` mapping table
    //! directly rather than running each variant through
    //! `IntoResponse + collect body` because the mapping is the
    //! load-bearing invariant — the JSON envelope wraps it
    //! consistently. Body shape is exercised in the http_smoke
    //! integration tests.

    use super::*;

    // ----- Status + kind mapping (Phase 4.0 + 4.F additions) -------------

    #[test]
    fn status_and_kind_phase_4_0_baseline() {
        assert_eq!(
            ApiError::Unauthorized.status_and_kind(),
            (StatusCode::UNAUTHORIZED, "Unauthorized"),
        );
        assert_eq!(
            ApiError::NotFound.status_and_kind(),
            (StatusCode::NOT_FOUND, "NotFound"),
        );
        assert_eq!(
            ApiError::AlreadyExists.status_and_kind(),
            (StatusCode::CONFLICT, "AlreadyExists"),
        );
        assert_eq!(
            ApiError::BadRequest("malformed uuid".to_string()).status_and_kind(),
            (StatusCode::BAD_REQUEST, "BadRequest"),
        );
        assert_eq!(
            ApiError::Internal.status_and_kind(),
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal"),
        );
    }

    #[test]
    fn state_conflict_is_409() {
        let err = ApiError::StateConflict {
            expected: "mutable",
            actual: "fixed".to_string(),
        };
        assert_eq!(err.status_and_kind(), (StatusCode::CONFLICT, "StateConflict"));
    }

    #[test]
    fn hash_mismatch_is_400() {
        let err = ApiError::HashMismatch {
            expected: "abc".to_string(),
            actual: "def".to_string(),
        };
        assert_eq!(
            err.status_and_kind(),
            (StatusCode::BAD_REQUEST, "HashMismatch"),
        );
    }

    #[test]
    fn forbidden_is_403() {
        let err = ApiError::Forbidden("scope objectrecords:write required".to_string());
        assert_eq!(err.status_and_kind(), (StatusCode::FORBIDDEN, "Forbidden"));
    }

    #[test]
    fn payload_too_large_is_413() {
        let err = ApiError::PayloadTooLarge {
            actual: 20_000_000,
            max: 10_485_760,
        };
        assert_eq!(
            err.status_and_kind(),
            (StatusCode::PAYLOAD_TOO_LARGE, "PayloadTooLarge"),
        );
    }

    // ----- DbError → ApiError translation --------------------------------

    #[test]
    fn db_state_mismatch_maps_to_state_conflict() {
        let dberr = DbError::StateMismatch {
            actual: "fixed".to_string(),
            requested: "mutable | snapshot",
        };
        let apierr: ApiError = dberr.into();
        match apierr {
            ApiError::StateConflict { expected, actual } => {
                assert_eq!(expected, "mutable | snapshot");
                assert_eq!(actual, "fixed");
            }
            other => panic!("expected StateConflict, got {other:?}"),
        }
    }

    #[test]
    fn db_already_exists_maps_to_already_exists() {
        let dberr = DbError::AlreadyExists(uuid::Uuid::nil());
        let apierr: ApiError = dberr.into();
        assert!(matches!(apierr, ApiError::AlreadyExists));
    }

    // ----- Error display (thiserror integration) -------------------------

    #[test]
    fn state_conflict_display_includes_both_states() {
        let err = ApiError::StateConflict {
            expected: "mutable",
            actual: "fixed".to_string(),
        };
        let s = err.to_string();
        assert!(s.contains("mutable"), "display should mention expected: {s}");
        assert!(s.contains("fixed"), "display should mention actual: {s}");
    }

    #[test]
    fn forbidden_display_includes_reason() {
        let err = ApiError::Forbidden("scope foo required".to_string());
        assert!(err.to_string().contains("scope foo required"));
    }
}

impl From<DbError> for ApiError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::AlreadyExists(_) => ApiError::AlreadyExists,
            // Phase 4.F: `transition_to_fixed` on an already-Fixed
            // record emits StateMismatch — surface to the client as
            // 409 StateConflict so callers can react (idempotent vs
            // genuine conflict). Other StateMismatch occurrences from
            // the conversion layer are not reachable here because
            // those callers handle the error before bubbling up.
            DbError::StateMismatch { actual, requested } => ApiError::StateConflict {
                expected: requested,
                actual,
            },
            // Conversion / schema-shape errors surface as Internal
            // because they would only happen on a corrupt row — the
            // caller should not be told which invariant tripped.
            DbError::UnknownState(_)
            | DbError::UnknownKind(_)
            | DbError::EmptyVersionChain(_)
            | DbError::MissingContentHash(_)
            | DbError::UnexpectedContentHash { .. }
            | DbError::MalformedHash(_)
            | DbError::SaveSnapshotRequiresExplicitMethod => {
                error!(?err, "db conversion error treated as Internal");
                ApiError::Internal
            }
            DbError::Surreal(inner) => {
                error!(?inner, "surreal backend error");
                ApiError::Internal
            }
        }
    }
}
