//! Wire-form DTOs for the two-table SurrealDB schema (decision #26).
//!
//! These types live in the db crate, **never** in
//! [`objectrecords_core`] — that's the whole point of decision #14
//! (core dependency-free) and axis C=(ii) (DTO + `TryFrom` boundary).
//!
//! Phase 3.0 ships the minimum surface needed for the
//! [`crate::convert`] round-trip:
//!
//! - [`PersistedRecord`] — the `record` row.
//! - [`PersistedVersion`] — one row per [`objectrecords_core::Version`]
//!   in the chain.
//! - [`PersistedBody`] — tagged enum for the version body (mirrors
//!   [`objectrecords_core::Body`]).
//! - [`PersistedState`] — the closed enum encoding the type-state.
//!
//! Persistence-layer metadata (`created_at`, `updated_at`, owner, etc.)
//! is deliberately omitted at this phase. They will arrive additively
//! in Phase 3.1+ when the live SurrealDB integration is wired up;
//! pre-introducing them now would force the [`From<&Record<S>>`]
//! implementations to fabricate timestamps, polluting the round-trip
//! property verified in `tests/persistence_round_trip.rs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb_types::SurrealValue;
use uuid::Uuid;

/// Wire-form representation of a `record` row.
///
/// The `kind` field is a free-form string for the open enum
/// [`objectrecords_core::Kind`]:
/// `"log" | "fix" | "dataset" | "asset" | "<other>"`. Custom kinds
/// outside the four built-ins must use the
/// [`objectrecords_core::RESERVED_CUSTOM_PREFIX`] (`creo:`) — the
/// [`crate::convert`] layer enforces neither the prefix nor casing,
/// since both are core-side invariants validated when the record is
/// rebuilt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedRecord {
    /// Record id (UUID v7, decision #10). Stable across the version
    /// chain.
    pub id: Uuid,
    /// Lifecycle state literal (decision #5 + Phase 1.5 decision #17).
    pub state: PersistedState,
    /// Open-enum kind label (decision #4).
    pub kind: String,
    /// Lowercase 64-char hex of the SHA-256 fossilization digest.
    /// `Some` for `Record<Fixed>` (decision #6), `None` for
    /// `Record<Mutable>` / `Record<Snapshot>`.
    pub content_hash: Option<String>,
    /// Wall-clock timestamp at record creation (decision #31).
    pub created_at: DateTime<Utc>,
    /// Wall-clock timestamp of the most recent state-changing write —
    /// the latest version append in particular. The repository keeps
    /// this in lock-step with the trailing version's `created_at` so a
    /// client can range-query "records modified since X" without a
    /// JOIN onto `version`.
    pub updated_at: DateTime<Utc>,
}

/// Wire-form representation of the `created_by` graph edge that
/// establishes attribution between a creator and a record (decision
/// #33, Paradigm Ⅱ — Phase 3.2 supersedes the Phase 3.1 `owner: string`
/// column).
///
/// In SurrealDB this is materialised as
/// `RELATE creo_user:<sub> -> created_by -> record:<uuid> SET at = <now>`.
/// The Phase 3.1 column-based `owner` field has been removed from
/// [`PersistedRecord`] entirely so the record body is pure data; the
/// attribution is a separate edge that travels alongside in
/// [`PersistedRow`] and [`crate::repository::RecordWithAttribution`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAttribution {
    /// CreoID Thing literal `"creo_user:<sub>"`. Cross-namespace
    /// strict typing (`record<creo_user>`) is held off because the
    /// `creo_user` table lives in the creo-memories namespace; the
    /// string preserves enough information for the api layer to
    /// reconstruct the Thing on demand.
    pub creator: String,
    /// Wall-clock timestamp at which this attribution edge was
    /// written. Independent from `record.created_at` because the
    /// edge can be re-issued (Phase 3.x may add a "transferred to"
    /// edge type, etc.); for the create path the two will coincide.
    pub at: DateTime<Utc>,
}

/// Wire-form representation of a `version` row.
///
/// The `record_id` field is the FK link back to
/// [`PersistedRecord::id`] (decision #26 — two-table normalised
/// schema, FK-linked). Phase 3.0 keeps this as a plain [`Uuid`]; the
/// Phase 3.1 SurrealDB repository will translate to/from
/// `surrealdb::sql::Thing` (`record:<uuid>` form) at the I/O boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedVersion {
    /// Version id (UUID v7, decision #10) — the inner `Uuid` of
    /// [`objectrecords_core::VersionId`].
    pub id: Uuid,
    /// FK to the parent record's [`PersistedRecord::id`].
    pub record_id: Uuid,
    /// Snapshot of the body at this version (Phase 1.5 annual rings).
    pub body: PersistedBody,
    /// Wall-clock timestamp at version creation (decision #31).
    /// Versions are immutable per decision #16 — there is no
    /// `updated_at` counterpart, by design.
    pub created_at: DateTime<Utc>,
}

/// Wire-form representation of [`objectrecords_core::Body`].
///
/// Tagged with `type = "inline" | "blob_ref"` so SurrealQL queries can
/// project either variant uniformly. The `Inline` bytes are kept as a
/// raw [`Vec<u8>`] for Phase 3.0 — SurrealDB serialises them as a CBOR
/// array of integers, which is acceptable while inline payloads are
/// expected to be small (Log / Fix / Dataset records). A future phase
/// may switch to `serde_bytes` for the CBOR bytes major type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersistedBody {
    /// Inline payload (Log / Fix / Dataset records — decision #8).
    Inline {
        /// Raw payload bytes.
        bytes: Vec<u8>,
    },
    /// Reference to a payload in the [`objectrecords-storage`] layer
    /// (Asset records — decision #8). The migratory-bird key
    /// convention (Phase 1.5 decision #15) applies: fluid while
    /// Mutable / Snapshot, rewritten to `/fixed/<sha256>` on `fix`.
    BlobRef {
        /// Storage key.
        key: String,
        /// Payload size in bytes.
        size: u64,
    },
}

/// Closed-enum encoding of the lifecycle state (decision #5 +
/// Phase 1.5 decision #17). Serialises as the lowercase literal
/// `"mutable" | "snapshot" | "fixed"`.
// SurrealValue derive intentionally omitted — Phase 4.D-fix-2 (2026-05-14)
// discovered that the v3 derive macro reads `#[surreal(...)]` attributes
// (not `#[serde(...)]`) and its only enum strategies are tagged-object
// shapes (`VariantKey` / `TagKey` / `TagContentKeys` / `Value-untagged`).
// None produce a raw `Value::String("mutable")`, but the schema declares
// `state ON record TYPE string`. The repository's internal
// `RecordRowDb.state` therefore carries a `String` instead, with
// boundary conversion via [`PersistedState::as_literal`] /
// [`PersistedState::from_literal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersistedState {
    /// Corresponds to [`objectrecords_core::Mutable`].
    Mutable,
    /// Corresponds to [`objectrecords_core::Snapshot`].
    Snapshot,
    /// Corresponds to [`objectrecords_core::Fixed`].
    Fixed,
}

impl PersistedState {
    /// Returns the lowercase literal stored in the SurrealDB row, for
    /// error reporting.
    #[must_use]
    pub fn as_literal(self) -> &'static str {
        match self {
            PersistedState::Mutable => "mutable",
            PersistedState::Snapshot => "snapshot",
            PersistedState::Fixed => "fixed",
        }
    }

    /// Parses the lowercase literal back into the typed enum. Inverse
    /// of [`Self::as_literal`].
    ///
    /// # Errors
    /// Returns the offending literal on unknown input.
    pub fn from_literal(s: &str) -> Result<Self, String> {
        match s {
            "mutable" => Ok(PersistedState::Mutable),
            "snapshot" => Ok(PersistedState::Snapshot),
            "fixed" => Ok(PersistedState::Fixed),
            other => Err(other.to_string()),
        }
    }
}

/// A single record together with its full version chain and attribution
/// edge — the unit a repository fetches when materialising a
/// `Record<S>` plus its provenance.
///
/// Phase 3.2 (decision #33, Paradigm Ⅱ) splits attribution out of the
/// record body: the row itself is pure data, and `attribution` carries
/// the `created_by` edge alongside. Defined as a struct (rather than a
/// tuple alias) so the `TryFrom`-style implementations in
/// [`crate::convert`] satisfy Rust's orphan rule: `Record<S>` is
/// foreign and a bare tuple is foreign, so the impl needs a local type
/// on at least one side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedRow {
    /// The `record` row.
    pub record: PersistedRecord,
    /// All `version` rows whose `record_id` matches `record.id`,
    /// ordered oldest-first to mirror
    /// [`objectrecords_core::Record::versions`].
    pub versions: Vec<PersistedVersion>,
    /// The `created_by` edge attribution (decision #33, Phase 3.2).
    /// Always present because every record has a creator (the api
    /// layer never persists without one — there is no "system-owned
    /// record" loophole).
    pub attribution: PersistedAttribution,
}
