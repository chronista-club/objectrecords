//! Conversions between [`objectrecords_core::Record`] and the wire-form
//! DTOs in [`crate::persisted`].
//!
//! This module is the formal expression of axis C=(ii) (decision #27):
//! the runtime → compile-time boundary lives **here**, not in core.
//!
//! # Forward (`Record<S>` → [`PersistedRow`])
//!
//! Phase 3.1 supersedes the three `From<&Record<S>>` impls of Phase 3.0
//! with a single generic free function [`record_to_persisted_row`]. The
//! switch is forced by decision #30 (Phase 3.1): every persisted row
//! carries a non-null `owner: String`, but the core [`Record<S>`] does
//! not know about owners (decision #18). A `From` impl cannot accept a
//! second argument, so the conversion **must** be a free function with
//! `(record, owner)` signature — the requirement is then enforced at
//! compile time.
//!
//! # Reverse ([`PersistedRow`] → `Record<S>`)
//!
//! `TryFrom<PersistedRow>` for each of `Record<Mutable>` /
//! `Record<Snapshot>` / `Record<Fixed>` is fallible because the row may
//! not satisfy the state's invariants (decision #6 — `Fixed` requires
//! `content_hash`, the others must not have one). The `owner` and the
//! timestamps are intentionally dropped on the way back: core's
//! [`Record<S>`] does not carry persistence-layer metadata.
//!
//! # State dispatch
//!
//! The generic forward function needs to know the state literal at
//! runtime even though `S` is a phantom marker at compile time. We
//! resolve this with the [`StateLiteral`] sealed trait, implemented for
//! each of [`Mutable`], [`Snapshot`], and [`Fixed`] in this crate. The
//! trait stays inside `objectrecords-db` to keep core dependency-free
//! (decision #14).
//!
//! # Kind serialisation
//!
//! Lowercase literals for the four built-ins (`log` / `fix` / `dataset`
//! / `asset`); any other string passes through verbatim as
//! [`Kind::Custom`]. Callers should follow `CLAUDE.md`'s convention of
//! prefixing custom kinds with
//! [`objectrecords_core::RESERVED_CUSTOM_PREFIX`] (`creo:`); the
//! conversion layer does not enforce this — that's a higher-layer
//! invariant.

use chrono::{DateTime, Utc};
use objectrecords_core::{
    Body, Fixed, Kind, Mutable, Record, Sha256Hash, Snapshot, State, Version, VersionId,
};

use crate::error::DbError;
use crate::persisted::{
    PersistedAttribution, PersistedBody, PersistedRecord, PersistedRow, PersistedState,
    PersistedVersion,
};

// =============================================================================
// State literal trait — db-crate-local mapping S -> PersistedState
// =============================================================================

/// Maps a core [`State`] to its [`PersistedState`] literal at compile
/// time.
///
/// Sealed inside this crate (`objectrecords-db`) so the mapping cannot
/// be extended downstream — that would require a corresponding row
/// state literal in the SurrealDB schema, which is out of scope for the
/// persistence layer.
pub trait StateLiteral: state_literal_sealed::Sealed {
    /// The wire-form literal for this state.
    const LITERAL: PersistedState;
}

impl StateLiteral for Mutable {
    const LITERAL: PersistedState = PersistedState::Mutable;
}
impl StateLiteral for Snapshot {
    const LITERAL: PersistedState = PersistedState::Snapshot;
}
impl StateLiteral for Fixed {
    const LITERAL: PersistedState = PersistedState::Fixed;
}

mod state_literal_sealed {
    use objectrecords_core::{Fixed, Mutable, Snapshot};
    pub trait Sealed {}
    impl Sealed for Mutable {}
    impl Sealed for Snapshot {}
    impl Sealed for Fixed {}
}

// =============================================================================
// Helpers
// =============================================================================

/// Parses a 64-char lowercase hex string into the raw 32-byte digest.
///
/// Mirrors the inverse of `Sha256Hash::Display`. Uppercase hex is
/// rejected to match the `Display` impl exactly — round-trip is
/// byte-equal.
fn parse_sha256_hex(s: &str) -> Result<[u8; 32], DbError> {
    if s.len() != 64 {
        return Err(DbError::MalformedHash(s.to_string()));
    }
    let mut out = [0u8; 32];
    let bytes = s.as_bytes();
    for i in 0..32 {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

/// Decodes a single ASCII hex nibble (lowercase only, matching
/// `Sha256Hash::Display`).
fn hex_nibble(c: u8) -> Result<u8, DbError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(DbError::MalformedHash(format!(
            "non-hex byte 0x{c:02x} in sha256",
        ))),
    }
}

/// Forward direction of [`Kind`].
fn kind_to_string(kind: &Kind) -> String {
    match kind {
        Kind::Log => "log".to_string(),
        Kind::Fix => "fix".to_string(),
        Kind::Dataset => "dataset".to_string(),
        Kind::Asset => "asset".to_string(),
        Kind::Custom(s) => s.clone(),
    }
}

/// Reverse direction of [`Kind`]. Built-ins win over custom; any other
/// string becomes [`Kind::Custom`].
///
/// Intentionally lossy: `Kind::Custom("log")` round-trips back as
/// `Kind::Log`. The `creo:` prefix convention exists to avoid this
/// collision.
fn kind_from_string(s: String) -> Kind {
    match s.as_str() {
        "log" => Kind::Log,
        "fix" => Kind::Fix,
        "dataset" => Kind::Dataset,
        "asset" => Kind::Asset,
        _ => Kind::Custom(s),
    }
}

/// Forward direction of [`Body`].
fn body_to_persisted(body: &Body) -> PersistedBody {
    match body {
        Body::Inline(bytes) => PersistedBody::Inline {
            bytes: bytes.clone(),
        },
        Body::BlobRef { key, size } => PersistedBody::BlobRef {
            key: key.clone(),
            size: *size,
        },
    }
}

/// Reverse direction of [`Body`]. Cannot fail.
fn body_from_persisted(body: PersistedBody) -> Body {
    match body {
        PersistedBody::Inline { bytes } => Body::Inline(bytes),
        PersistedBody::BlobRef { key, size } => Body::BlobRef { key, size },
    }
}

/// Forward direction of one [`Version`] within a known parent record id
/// and timestamp.
fn version_to_persisted(
    version: &Version,
    record_id: uuid::Uuid,
    created_at: DateTime<Utc>,
) -> PersistedVersion {
    PersistedVersion {
        id: version.id.0,
        record_id,
        body: body_to_persisted(&version.body),
        created_at,
    }
}

/// Reverse direction of one [`Version`]. The `created_at` field is
/// intentionally dropped — core does not carry timestamps.
fn version_from_persisted(persisted: PersistedVersion) -> Version {
    Version {
        id: VersionId(persisted.id),
        body: body_from_persisted(persisted.body),
    }
}

/// Common reverse-path validation: state literal matches `expected`.
fn validate_state(record: &PersistedRecord, expected: PersistedState) -> Result<(), DbError> {
    if record.state != expected {
        return Err(DbError::StateMismatch {
            actual: record.state.as_literal().to_string(),
            requested: expected.as_literal(),
        });
    }
    Ok(())
}

/// Common reverse-path validation: version chain non-empty.
fn ensure_versions_present(
    record_id: uuid::Uuid,
    versions: &[PersistedVersion],
) -> Result<(), DbError> {
    if versions.is_empty() {
        return Err(DbError::EmptyVersionChain(record_id));
    }
    Ok(())
}

// =============================================================================
// Forward: Record<S> -> PersistedRow (free function, owner-aware)
// =============================================================================

/// Builds a [`PersistedRow`] from `record` and `creator`.
///
/// Phase 3.2 supersedes the Phase 3.1 form: the `owner: String`
/// column on `PersistedRecord` has been moved out into a
/// [`PersistedAttribution`] edge sibling (decision #33, Paradigm Ⅱ).
/// The function still takes a single `creator` argument — the CreoID
/// Thing literal `"creo_user:<sub>"` — but it now flows into
/// `row.attribution.creator` instead of `row.record.owner`.
///
/// The timestamps (`record.created_at`, `record.updated_at`,
/// `version.created_at`, `attribution.at`) are populated with a
/// single `Utc::now()` — repository code that needs deterministic
/// timestamps for testing should mutate the resulting row before
/// insert.
///
/// State dispatch goes through [`StateLiteral`]; `content_hash` is
/// projected via the public [`Record::content_hash`] accessor
/// (returns `None` for `Mutable` / `Snapshot` and `Some` for
/// `Fixed`).
#[must_use]
pub fn record_to_persisted_row<S>(record: &Record<S>, creator: String) -> PersistedRow
where
    S: State + StateLiteral,
{
    let now = Utc::now();
    let id = record.id();
    let kind = kind_to_string(record.kind());
    let content_hash = record.content_hash().map(|h| h.to_string());
    let versions = record
        .versions()
        .iter()
        .map(|v| version_to_persisted(v, id, now))
        .collect();
    PersistedRow {
        record: PersistedRecord {
            id,
            state: S::LITERAL,
            kind,
            content_hash,
            created_at: now,
            updated_at: now,
        },
        versions,
        attribution: PersistedAttribution { creator, at: now },
    }
}

// =============================================================================
// Reverse: PersistedRow -> Record<S>
// =============================================================================

impl TryFrom<PersistedRow> for Record<Mutable> {
    type Error = DbError;

    fn try_from(row: PersistedRow) -> Result<Self, DbError> {
        // attribution is intentionally ignored on the way back into
        // core::Record — core stays free of provenance metadata
        // (decision #14 + Paradigm Ⅱ keeps record-as-pure-data).
        let PersistedRow {
            record,
            versions,
            attribution: _,
        } = row;
        validate_state(&record, PersistedState::Mutable)?;
        if record.content_hash.is_some() {
            return Err(DbError::UnexpectedContentHash {
                state: "Mutable",
                id: record.id,
            });
        }
        ensure_versions_present(record.id, &versions)?;

        let id = record.id;
        let kind = kind_from_string(record.kind);
        let core_versions = versions.into_iter().map(version_from_persisted).collect();
        Ok(Record::<Mutable>::from_parts(id, kind, core_versions))
    }
}

impl TryFrom<PersistedRow> for Record<Snapshot> {
    type Error = DbError;

    fn try_from(row: PersistedRow) -> Result<Self, DbError> {
        // attribution is intentionally ignored on the way back into
        // core::Record — core stays free of provenance metadata
        // (decision #14 + Paradigm Ⅱ keeps record-as-pure-data).
        let PersistedRow {
            record,
            versions,
            attribution: _,
        } = row;
        validate_state(&record, PersistedState::Snapshot)?;
        if record.content_hash.is_some() {
            return Err(DbError::UnexpectedContentHash {
                state: "Snapshot",
                id: record.id,
            });
        }
        ensure_versions_present(record.id, &versions)?;

        let id = record.id;
        let kind = kind_from_string(record.kind);
        let core_versions = versions.into_iter().map(version_from_persisted).collect();
        Ok(Record::<Snapshot>::from_parts(id, kind, core_versions))
    }
}

impl TryFrom<PersistedRow> for Record<Fixed> {
    type Error = DbError;

    fn try_from(row: PersistedRow) -> Result<Self, DbError> {
        // attribution is intentionally ignored on the way back into
        // core::Record — core stays free of provenance metadata
        // (decision #14 + Paradigm Ⅱ keeps record-as-pure-data).
        let PersistedRow {
            record,
            versions,
            attribution: _,
        } = row;
        validate_state(&record, PersistedState::Fixed)?;
        let hash_hex = record
            .content_hash
            .as_ref()
            .ok_or(DbError::MissingContentHash(record.id))?;
        let hash = Sha256Hash(parse_sha256_hex(hash_hex)?);
        ensure_versions_present(record.id, &versions)?;

        let id = record.id;
        let kind = kind_from_string(record.kind);
        let core_versions = versions.into_iter().map(version_from_persisted).collect();
        Ok(Record::<Fixed>::from_parts(id, kind, hash, core_versions))
    }
}

// =============================================================================
// State-agnostic dispatch into AnyRecord
// =============================================================================

/// Dispatches a [`PersistedRow`] into the matching [`crate::AnyRecord`]
/// variant based on its `state` literal.
///
/// Used by [`crate::repository::SurrealRecordRepository::find`] to
/// reconstruct the typed `Record<S>` without the caller needing to
/// know in advance which state was stored. The fallibility is the
/// same as the per-state `TryFrom` impls above.
pub fn persisted_row_to_any(row: PersistedRow) -> Result<crate::AnyRecord, DbError> {
    match row.record.state {
        PersistedState::Mutable => Ok(crate::AnyRecord::Mutable(Record::<Mutable>::try_from(row)?)),
        PersistedState::Snapshot => {
            Ok(crate::AnyRecord::Snapshot(Record::<Snapshot>::try_from(row)?))
        }
        PersistedState::Fixed => Ok(crate::AnyRecord::Fixed(Record::<Fixed>::try_from(row)?)),
    }
}
