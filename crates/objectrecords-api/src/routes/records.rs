//! `/records/:id` route group (Phase 4.0 + 4.F).
//!
//! Phase 4.0 baseline: `GET /records/:id`.
//! Phase 4.F additions: `POST /records` (create Mutable), `PUT
//! /records/:id` (add version) and `POST /records/:id/fix` will land
//! incrementally — POST /records is the first dogfood enabler since
//! creo-memories needs it to attach a memory to a new record.
//!
//! Wire shape: requests use `inline_b64` (base64 of raw bytes) or
//! `blob_ref` (`{ key, size }`) for `body`. Responses re-use
//! [`PersistedRow`] verbatim (Sub-Q4 default — deferred DTO).
//!
//! Auth + scope gating is applied at [`crate::build_router`], not
//! per-route here.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::Utc;
use objectrecords_core::{Body, Kind, Record, Sha256Hash, State as CoreState};
use objectrecords_db::{
    AnyRecord, PersistedAttribution, PersistedBody, PersistedRecord, PersistedRow,
    PersistedState, PersistedVersion, RecordRepository, RecordWithAttribution,
};
use objectrecords_storage::BlobStorage;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::error;
use uuid::Uuid;

use crate::AppState;
use crate::auth::Claims;
use crate::error::ApiError;

async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PersistedRow>, ApiError> {
    let rwa = state
        .repo
        .find(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(record_with_attribution_to_row(rwa)))
}

/// Re-projects a [`RecordWithAttribution`] into a [`PersistedRow`] for
/// JSON serialisation.
///
/// Phase 4.0 deliberately mirrors the wire form of the db layer —
/// see Sub-Q4 default. Phase 4.x may swap this for a custom
/// `RecordResponse` DTO with a more browser-friendly shape (e.g.,
/// base64-encoded `Inline.bytes`).
fn record_with_attribution_to_row(rwa: RecordWithAttribution) -> PersistedRow {
    let RecordWithAttribution {
        record,
        attribution,
    } = rwa;
    match record {
        AnyRecord::Mutable(r) => build_row(&r, PersistedState::Mutable, None, attribution),
        AnyRecord::Snapshot(r) => build_row(&r, PersistedState::Snapshot, None, attribution),
        AnyRecord::Fixed(r) => {
            let hash_hex = r
                .content_hash()
                .map(Sha256Hash::to_string)
                .expect("Record<Fixed> must carry a content_hash");
            build_row(&r, PersistedState::Fixed, Some(hash_hex), attribution)
        }
    }
}

/// Common projection of a typed [`Record<S>`] back into a
/// [`PersistedRow`]. Mirrors `record_to_persisted_row` from the db
/// crate but reuses the persistence-layer timestamps recovered via
/// `find` — we synthesise a "now-shaped" approximation since
/// [`Record<S>`] does not carry timestamps; the `created_at` /
/// `updated_at` of the row are populated from the attribution edge
/// (best signal we have at this layer until Phase 4.x adds richer
/// metadata fetching).
fn build_row<S>(
    record: &Record<S>,
    state: PersistedState,
    content_hash: Option<String>,
    attribution: PersistedAttribution,
) -> PersistedRow
where
    S: CoreState,
{
    let id = record.id();
    let now = attribution.at;
    let versions: Vec<PersistedVersion> = record
        .versions()
        .iter()
        .map(|v| PersistedVersion {
            id: v.id.0,
            record_id: id,
            body: match &v.body {
                objectrecords_core::Body::Inline(bytes) => PersistedBody::Inline {
                    bytes: bytes.clone(),
                },
                objectrecords_core::Body::BlobRef { key, size } => PersistedBody::BlobRef {
                    key: key.clone(),
                    size: *size,
                },
            },
            created_at: now,
        })
        .collect();
    PersistedRow {
        record: PersistedRecord {
            id,
            state,
            kind: kind_to_string(record.kind()),
            content_hash,
            created_at: now,
            updated_at: now,
        },
        versions,
        attribution,
    }
}

fn kind_to_string(kind: &objectrecords_core::Kind) -> String {
    match kind {
        objectrecords_core::Kind::Log => "log".to_string(),
        objectrecords_core::Kind::Fix => "fix".to_string(),
        objectrecords_core::Kind::Dataset => "dataset".to_string(),
        objectrecords_core::Kind::Asset => "asset".to_string(),
        objectrecords_core::Kind::Custom(s) => s.clone(),
    }
}

// =============================================================================
// POST /records — create a new `Record<Mutable>` (Phase 4.F / Stage 3)
// =============================================================================

/// JSON body for `POST /records`.
///
/// The `body` field is one of:
/// - `{ "inline_b64": "<base64 of bytes>" }` — Log / Fix / Dataset etc.
/// - `{ "blob_ref": { "key": "/assets/<sha256>", "size": N } }` — Asset.
///
/// `kind` is an open-enum string. Reserved prefix `creo:` (decision
/// #4 + internal design notes) is accepted at the API layer;
/// only the `creo` namespace itself enforces ownership of that prefix.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRecordRequest {
    /// Kind label. Predefined: `log` / `fix` / `dataset` / `asset`.
    /// Anything else becomes [`Kind::Custom`].
    pub kind: String,
    /// Initial body (becomes the only [`objectrecords_core::Version`]
    /// on the freshly minted record).
    pub body: BodyInput,
}

/// JSON body fragment representing an inline payload encoded as
/// base64 (RFC 4648 STANDARD alphabet, no URL-safe).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineB64Input {
    /// Standard base64 of the bytes (`base64::engine::general_purpose::STANDARD`).
    /// Empty string is rejected at parse time below.
    pub inline_b64: String,
}

/// JSON body fragment representing a reference to a previously
/// uploaded blob (typically the response of `POST /assets`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobRefInput {
    /// Inner shape of the blob_ref tag.
    pub blob_ref: BlobRefInner,
}

/// `key` + `size` pair held by [`BlobRefInput::blob_ref`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobRefInner {
    /// Storage key — typically `/assets/<sha256>` for a Mutable /
    /// Snapshot record, or `/fixed/<sha256>` after a `fix()` (the
    /// caller must NOT pre-fill `/fixed/` here; `fix()` performs
    /// that rewrite atomically at decision #15).
    pub key: String,
    /// Payload size in bytes. The server cross-checks against
    /// [`objectrecords_storage::BlobStorage::head`] when the storage
    /// backend is configured.
    pub size: u64,
}

/// Tagged-by-single-field-key untagged enum: deserialiser tries each
/// variant in order and picks the first that matches.
///
/// - `{"inline_b64": "..."}` → [`Self::InlineB64`]
/// - `{"blob_ref": { "key": "...", "size": N }}` → [`Self::BlobRef`]
///
/// A body with neither key (or both) is a deserialize error =>
/// surfaced as `BadRequest` by the axum `Json` extractor.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BodyInput {
    /// Inline bytes, base64-encoded.
    InlineB64(InlineB64Input),
    /// Reference to an object in the blob store.
    BlobRef(BlobRefInput),
}

impl BodyInput {
    /// Decodes the input into a [`objectrecords_core::Body`].
    ///
    /// # Errors
    ///
    /// - [`ApiError::BadRequest`] when `inline_b64` is empty or not
    ///   valid base64.
    pub(crate) fn into_core_body(self) -> Result<Body, ApiError> {
        match self {
            Self::InlineB64(InlineB64Input { inline_b64 }) => {
                if inline_b64.is_empty() {
                    return Err(ApiError::BadRequest(
                        "body.inline_b64 must not be an empty string".to_string(),
                    ));
                }
                let bytes = BASE64_STANDARD
                    .decode(inline_b64.as_bytes())
                    .map_err(|e| ApiError::BadRequest(format!("body.inline_b64 base64 decode: {e}")))?;
                Ok(Body::Inline(bytes))
            }
            Self::BlobRef(BlobRefInput { blob_ref }) => {
                if blob_ref.key.is_empty() {
                    return Err(ApiError::BadRequest(
                        "body.blob_ref.key must not be empty".to_string(),
                    ));
                }
                Ok(Body::BlobRef {
                    key: blob_ref.key,
                    size: blob_ref.size,
                })
            }
        }
    }
}

/// Parses the `kind` field into a [`Kind`] open-enum.
///
/// Predefined: `log` / `fix` / `dataset` / `asset` (decision #4).
/// Anything else becomes `Kind::Custom(s)` — `creo:` prefix is
/// accepted but ownership of that namespace is enforced by the creo
/// side, not by OR (per internal design notes).
pub(crate) fn parse_kind(raw: &str) -> Result<Kind, ApiError> {
    if raw.is_empty() {
        return Err(ApiError::BadRequest("kind must not be empty".to_string()));
    }
    Ok(match raw {
        "log" => Kind::Log,
        "fix" => Kind::Fix,
        "dataset" => Kind::Dataset,
        "asset" => Kind::Asset,
        custom => Kind::Custom(custom.to_string()),
    })
}

async fn post_one(
    State(state): State<AppState>,
    claims: Option<axum::extract::Extension<Arc<Claims>>>,
    Json(req): Json<CreateRecordRequest>,
) -> Result<Response, ApiError> {
    let kind = parse_kind(&req.kind)?;
    let core_body = req.body.into_core_body()?;
    let record = Record::new(kind, core_body);

    let creator = creator_from_claims(claims.as_deref());
    state
        .repo
        .save(&record, &creator)
        .await
        .map_err(ApiError::from)?;

    let now = Utc::now();
    let row = build_row(
        &record,
        PersistedState::Mutable,
        None,
        PersistedAttribution {
            creator,
            at: now,
        },
    );
    Ok((
        StatusCode::CREATED,
        [(
            axum::http::header::LOCATION,
            format!("/records/{}", row.record.id),
        )],
        Json(row),
    )
        .into_response())
}

/// Builds the `creo_user:<sub>` Thing literal stored in
/// [`PersistedAttribution::creator`] (decision #33).
///
/// When no JWT is present (scratch / no-auth mode), an explicit
/// `"creo_user:anonymous"` placeholder is used so the schema
/// constraint (`creator` is required) remains satisfied. live mode
/// always provides claims, so the anonymous branch only fires on
/// scratch deploys.
fn creator_from_claims(claims: Option<&Arc<Claims>>) -> String {
    match claims {
        Some(c) => format!("creo_user:{}", c.sub),
        None => "creo_user:anonymous".to_string(),
    }
}

// =============================================================================
// PUT /records/:id — append a version (Phase 4.F / Stage 3)
// =============================================================================

/// JSON body for `PUT /records/:id`.
///
/// Same `body` shape as [`CreateRecordRequest`] but without `kind`
/// (which is immutable once the record exists). Snapshot / Fixed
/// records reject this operation with `409 StateConflict` — only
/// Mutable records grow new versions.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRecordRequest {
    /// New version body (mirrors [`CreateRecordRequest::body`]).
    pub body: BodyInput,
}

async fn put_one(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateRecordRequest>,
) -> Result<Json<PersistedRow>, ApiError> {
    // 1. Existence + state precondition.
    let rwa = state
        .repo
        .find(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    match rwa.record {
        AnyRecord::Mutable(_) => {} // ok
        AnyRecord::Snapshot(_) => {
            return Err(ApiError::StateConflict {
                expected: "mutable",
                actual: "snapshot".to_string(),
            });
        }
        AnyRecord::Fixed(_) => {
            return Err(ApiError::StateConflict {
                expected: "mutable",
                actual: "fixed".to_string(),
            });
        }
    }
    // 2. Body validation + decode.
    let core_body = req.body.into_core_body()?;
    // 3. Atomic add_version (db layer handles the transaction).
    state
        .repo
        .add_version(id, core_body)
        .await
        .map_err(ApiError::from)?;
    // 4. Re-fetch + project for response. The re-find is a single
    //    round-trip and gives us the canonical view (including the
    //    newly-minted version id) without re-implementing assembly.
    let rwa = state
        .repo
        .find(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(record_with_attribution_to_row(rwa)))
}

// =============================================================================
// POST /records/:id/fix — fossilize (Phase 4.F / Stage 3)
// =============================================================================

/// JSON body for `POST /records/:id/fix`.
///
/// The `content_hash` must be the lowercase hex of the SHA-256 of
/// the **trailing version's** body bytes:
/// - For [`Body::Inline`]: hash of the inline bytes.
/// - For [`Body::BlobRef`]: hash of the bytes currently stored at
///   `body.key` — the server fetches via [`BlobStorage::get`] and
///   re-hashes for verification.
///
/// Mismatch returns `400 HashMismatch`. Already-Fixed returns
/// `409 StateConflict`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixRecordRequest {
    /// Hex-encoded SHA-256 over the trailing version's payload bytes.
    /// Lower-case, no separator, exactly 64 chars.
    pub content_hash: String,
}

async fn fix_one(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<FixRecordRequest>,
) -> Result<Json<PersistedRow>, ApiError> {
    let claimed = parse_sha256_hex(&req.content_hash)?;

    // 1. Find + state precondition.
    let rwa = state
        .repo
        .find(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let last_body: Body = match &rwa.record {
        AnyRecord::Mutable(r) => trailing_body(r.versions())?,
        AnyRecord::Snapshot(r) => trailing_body(r.versions())?,
        AnyRecord::Fixed(_) => {
            return Err(ApiError::StateConflict {
                expected: "mutable | snapshot",
                actual: "fixed".to_string(),
            });
        }
    };

    // 2. Compute hash from current trailing body.
    let computed = match &last_body {
        Body::Inline(bytes) => sha256_of(bytes),
        Body::BlobRef { key, .. } => {
            let storage = state.storage.as_ref().ok_or_else(|| {
                error!(
                    "fix() on BlobRef body but storage is unconfigured; \
                     deploy missing OBJECTRECORDS_S3_* env vars"
                );
                ApiError::Internal
            })?;
            let bytes = storage.get(key).await.map_err(|err| {
                error!(?err, %key, "storage.get failed during fix() hash verify");
                ApiError::Internal
            })?;
            sha256_of(&bytes)
        }
    };
    if computed != claimed {
        return Err(ApiError::HashMismatch {
            expected: computed.to_string(),
            actual: claimed.to_string(),
        });
    }

    // 3. For BlobRef: copy /assets/<hash> → /fixed/<hash> at storage
    //    layer (idempotent via put_if_absent). The db transition next
    //    rewrites the version's key to `/fixed/<hash>` (decision #15).
    if let Body::BlobRef { key, .. } = &last_body
        && let Some(storage) = state.storage.as_ref()
    {
        let bytes = storage.get(key).await.map_err(|err| {
            error!(?err, %key, "storage.get failed before /fixed copy");
            ApiError::Internal
        })?;
        let fixed_key = format!("/fixed/{claimed}");
        let _outcome = storage
            .put_if_absent(&fixed_key, bytes)
            .await
            .map_err(|err| {
                error!(?err, %fixed_key, "storage.put_if_absent /fixed/* failed");
                ApiError::Internal
            })?;
    }

    // 4. db transition: state=Fixed + content_hash + last BlobRef.key
    //    rewrite. Atomic at the SurrealDB transaction level.
    state
        .repo
        .transition_to_fixed(id, claimed)
        .await
        .map_err(ApiError::from)?;

    // 5. Project for response.
    let rwa = state
        .repo
        .find(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(record_with_attribution_to_row(rwa)))
}

/// Extracts a clone of the trailing version's body. Returns
/// [`ApiError::Internal`] when the version chain is unexpectedly
/// empty (db invariant violation — every record has ≥1 version).
fn trailing_body(versions: &[objectrecords_core::Version]) -> Result<Body, ApiError> {
    let last = versions.last().ok_or_else(|| {
        error!("record has empty version chain — db invariant violated");
        ApiError::Internal
    })?;
    Ok(last.body.clone())
}

/// Computes SHA-256 over `bytes` and returns a [`Sha256Hash`].
fn sha256_of(bytes: &[u8]) -> Sha256Hash {
    let digest = Sha256::digest(bytes);
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&digest);
    Sha256Hash(arr)
}

/// Parses the lowercase 64-char hex string into a [`Sha256Hash`].
///
/// Strict validation matches the [`Sha256Hash`] display format:
/// length == 64, every byte parses as `u8::from_str_radix(_, 16)`.
/// Mixed case (`"ABC...DEF"`) is rejected because the storage key
/// convention requires lowercase and silent re-casing would break
/// content-addressing.
pub(crate) fn parse_sha256_hex(s: &str) -> Result<Sha256Hash, ApiError> {
    if s.len() != 64 {
        return Err(ApiError::BadRequest(format!(
            "content_hash must be 64 hex chars, got {}",
            s.len()
        )));
    }
    if s != s.to_lowercase() {
        return Err(ApiError::BadRequest(
            "content_hash must be lowercase hex".to_string(),
        ));
    }
    let mut bytes = [0u8; 32];
    for (i, slot) in bytes.iter_mut().enumerate() {
        let hex_byte = &s[2 * i..2 * i + 2];
        *slot = u8::from_str_radix(hex_byte, 16).map_err(|e| {
            ApiError::BadRequest(format!(
                "content_hash invalid hex at byte {i} (`{hex_byte}`): {e}"
            ))
        })?;
    }
    Ok(Sha256Hash(bytes))
}

/// Returns the `Router` fragment for the records endpoints.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/records/{id}", get(get_one).put(put_one))
        .route("/records/{id}/fix", post(fix_one))
        .route("/records", post(post_one))
}

#[cfg(test)]
mod tests {
    //! Small-tier unit tests for POST /records body parsing /
    //! kind parsing / claims-to-creator helper. Handler integration
    //! (round-trip against SurrealDB) lives in the env-gated Medium
    //! tier (`tests/http_smoke.rs`).

    use super::*;

    // ----- BodyInput deserialization -----

    #[test]
    fn body_input_inline_b64_deserializes() {
        let json = r#"{"inline_b64":"aGVsbG8="}"#;
        let body: BodyInput = serde_json::from_str(json).unwrap();
        match body {
            BodyInput::InlineB64(InlineB64Input { inline_b64 }) => {
                assert_eq!(inline_b64, "aGVsbG8=");
            }
            other => panic!("expected InlineB64, got {other:?}"),
        }
    }

    #[test]
    fn body_input_blob_ref_deserializes() {
        let json = r#"{"blob_ref":{"key":"/assets/abc","size":42}}"#;
        let body: BodyInput = serde_json::from_str(json).unwrap();
        match body {
            BodyInput::BlobRef(BlobRefInput { blob_ref }) => {
                assert_eq!(blob_ref.key, "/assets/abc");
                assert_eq!(blob_ref.size, 42);
            }
            other => panic!("expected BlobRef, got {other:?}"),
        }
    }

    #[test]
    fn body_input_rejects_unknown_keys() {
        let json = r#"{"hopeful_typo":"x"}"#;
        let result: Result<BodyInput, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "untagged enum must reject keys that match neither variant"
        );
    }

    // ----- into_core_body -----

    #[test]
    fn into_core_body_decodes_base64() {
        let body = BodyInput::InlineB64(InlineB64Input {
            inline_b64: "aGVsbG8=".to_string(),
        });
        let core = body.into_core_body().unwrap();
        match core {
            Body::Inline(bytes) => assert_eq!(bytes.as_slice(), b"hello"),
            Body::BlobRef { .. } => panic!("expected Inline"),
        }
    }

    #[test]
    fn into_core_body_rejects_empty_inline_b64() {
        let body = BodyInput::InlineB64(InlineB64Input {
            inline_b64: String::new(),
        });
        match body.into_core_body() {
            Err(ApiError::BadRequest(msg)) => assert!(msg.contains("empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn into_core_body_rejects_invalid_base64() {
        let body = BodyInput::InlineB64(InlineB64Input {
            inline_b64: "!!!not valid base64!!!".to_string(),
        });
        assert!(matches!(body.into_core_body(), Err(ApiError::BadRequest(_))));
    }

    #[test]
    fn into_core_body_blob_ref_passes_through() {
        let body = BodyInput::BlobRef(BlobRefInput {
            blob_ref: BlobRefInner {
                key: "/assets/sha256hex".to_string(),
                size: 100,
            },
        });
        let core = body.into_core_body().unwrap();
        match core {
            Body::BlobRef { key, size } => {
                assert_eq!(key, "/assets/sha256hex");
                assert_eq!(size, 100);
            }
            Body::Inline(_) => panic!("expected BlobRef"),
        }
    }

    #[test]
    fn into_core_body_rejects_empty_blob_ref_key() {
        let body = BodyInput::BlobRef(BlobRefInput {
            blob_ref: BlobRefInner {
                key: String::new(),
                size: 0,
            },
        });
        assert!(matches!(body.into_core_body(), Err(ApiError::BadRequest(_))));
    }

    // ----- parse_kind -----

    #[test]
    fn parse_kind_predefined_variants() {
        assert!(matches!(parse_kind("log").unwrap(), Kind::Log));
        assert!(matches!(parse_kind("fix").unwrap(), Kind::Fix));
        assert!(matches!(parse_kind("dataset").unwrap(), Kind::Dataset));
        assert!(matches!(parse_kind("asset").unwrap(), Kind::Asset));
    }

    #[test]
    fn parse_kind_custom_passes_through() {
        match parse_kind("creo:memory").unwrap() {
            Kind::Custom(s) => assert_eq!(s, "creo:memory"),
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn parse_kind_empty_rejected() {
        assert!(matches!(parse_kind(""), Err(ApiError::BadRequest(_))));
    }

    // ----- creator_from_claims -----

    #[test]
    fn creator_from_claims_uses_sub_when_present() {
        let claims = Arc::new(Claims {
            sub: "auth0|abc123".to_string(),
            aud: serde_json::Value::String("a".to_string()),
            iss: "i".to_string(),
            exp: 0,
            iat: None,
            act: None,
            scope: String::new(),
        });
        let creator = creator_from_claims(Some(&claims));
        assert_eq!(creator, "creo_user:auth0|abc123");
    }

    #[test]
    fn creator_from_claims_anonymous_when_missing() {
        let creator = creator_from_claims(None);
        assert_eq!(creator, "creo_user:anonymous");
    }

    // ----- parse_sha256_hex (POST /records/:id/fix) -----

    #[test]
    fn parse_sha256_hex_valid_lowercase() {
        let s = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let h = parse_sha256_hex(s).expect("valid lowercase 64-char hex");
        assert_eq!(h.to_string(), s);
    }

    #[test]
    fn parse_sha256_hex_rejects_uppercase() {
        let s = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(matches!(parse_sha256_hex(s), Err(ApiError::BadRequest(_))));
    }

    #[test]
    fn parse_sha256_hex_rejects_wrong_length() {
        assert!(matches!(parse_sha256_hex(""), Err(ApiError::BadRequest(_))));
        assert!(matches!(parse_sha256_hex("abc"), Err(ApiError::BadRequest(_))));
        // 63 chars — exactly one short
        assert!(matches!(
            parse_sha256_hex(&"a".repeat(63)),
            Err(ApiError::BadRequest(_))
        ));
        // 65 chars — exactly one over
        assert!(matches!(
            parse_sha256_hex(&"a".repeat(65)),
            Err(ApiError::BadRequest(_))
        ));
    }

    #[test]
    fn parse_sha256_hex_rejects_non_hex_chars() {
        // 64 chars but with 'z' in position 0
        let s = format!("z{}", "a".repeat(63));
        assert!(matches!(parse_sha256_hex(&s), Err(ApiError::BadRequest(_))));
    }

    #[test]
    fn parse_sha256_hex_zero_hash_round_trip() {
        let zero = "0".repeat(64);
        let h = parse_sha256_hex(&zero).expect("zero hash is valid");
        assert_eq!(h.0, [0u8; 32]);
        assert_eq!(h.to_string(), zero);
    }

    // ----- sha256_of helper -----

    #[test]
    fn sha256_of_matches_rfc_vector() {
        // RFC 6234 test vector: SHA-256("hello") = 2cf24dba...
        let h = sha256_of(b"hello");
        assert_eq!(
            h.to_string(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha256_of_empty_input() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let h = sha256_of(b"");
        assert_eq!(
            h.to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_of_round_trip_with_parse() {
        let h = sha256_of(b"deterministic");
        let parsed = parse_sha256_hex(&h.to_string()).expect("round-trip valid");
        assert_eq!(parsed.0, h.0);
    }
}
