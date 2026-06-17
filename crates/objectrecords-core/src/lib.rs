//! Object Records — type-level skeleton.
//!
//! This crate is the dependency-free core of Object Records, the internal
//! immutable data store of the Creo ecosystem. It encodes the decision chain
//! pinned in `CLAUDE.md` (decisions 1-13 + Custom prefix branch + Phase 1.5
//! decisions) directly into Rust types so the compiler becomes the first
//! guardrail.
//!
//! The design SSOT lives in the `objectrecords.io` atlas of `creo-memories`
//!. Implementation choices in this crate must
//! cite the relevant decision memory id.
//!
//! # Topology — three states (Phase 1.5)
//!
//! ```text
//!   Mutable ──update──▶ Mutable
//!      │ snapshot           │
//!      │  (&self, clone)    │ fix(hash)
//!      ▼                    │
//!   Snapshot ───────────────┴──▶ Fixed
//!                fix(hash)
//! ```
//!
//! - [`Mutable`] — evolving record. `update` appends a new [`Version`] to the
//!   chain (decision #1). `snapshot(&self)` produces an independent
//!   [`Snapshot`] copy without consuming `self`, so the Mutable continues to
//!   evolve (camera-shutter metaphor, Phase 1.5).
//! - [`Snapshot`] — frozen-but-not-fossilized. No `update`, but `fix` is
//!   available. Useful for preview / draft / staging / parallel review use
//!   cases where you want a stable view while edits continue.
//! - [`Fixed`] — irreversibly fossilized via [`Record::fix`]. No mutating
//!   methods at all (compile-time guarantee).
//!
//! # Phase 1.5 design — fossilization metaphor
//!
//! The `fix()` API is shaped after permineralization in nature: external
//! minerals (the [`Sha256Hash`] argument) are injected into the organism (the
//! record) to produce an irreversibly hardened fossil. The caller (typically
//! the `objectrecords-storage` layer) computes the digest over the body
//! bytes and supplies it to `fix`. core stays dependency-free.
//!
//! Storage keys follow the migratory-bird metaphor: while Mutable / Snapshot,
//! the blob key is fluid (storage layer's responsibility). At fix time, core
//! rewrites the trailing version's `BlobRef.key` to `/fixed/<sha256>` so the
//! dedup commitment of decision #11 is honoured at the type-state boundary.
//!
//! Each [`Version`] carries its own body snapshot ("annual rings", Phase 1.5
//! user heuristic — fossilization preserves the full growth history rather
//! than only the terminal state).

use std::marker::PhantomData;

use uuid::Uuid;

// =============================================================================
// State (decision #5: type-state pattern, Phase 1.5: three states)
// =============================================================================

/// Sealed marker trait for the lifecycle state of a [`Record`].
///
/// Only [`Mutable`], [`Snapshot`], and [`Fixed`] may implement this trait. The
/// seal prevents downstream crates from inventing new states that bypass the
/// immutability guarantees of the design.
pub trait State: sealed::Sealed {}

/// Marker type for records that may be updated in place.
///
/// Each `update` appends a new [`Version`] to the chain (decision #1). A
/// `Mutable` record can transition to [`Snapshot`] via [`Record::snapshot`]
/// (clone, non-consuming) or directly to [`Fixed`] via [`Record::fix`]
/// (consuming, irreversible).
#[derive(Debug, Clone, Copy)]
pub struct Mutable;

/// Marker type for records that have been frozen for review but not yet
/// fossilized.
///
/// `Record<Snapshot>` has no `update` method (so it cannot mutate) but does
/// have `fix` (so it can be fossilized). Use cases: preview, draft, staging,
/// parallel review while the source [`Mutable`] continues to evolve.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot;

/// Marker type for records that have been fossilized via [`Record::fix`].
///
/// `Record<Fixed>` deliberately has **no** mutating methods, so any attempt
/// to alter a fixed record is a compile error rather than a runtime check.
#[derive(Debug, Clone, Copy)]
pub struct Fixed;

impl State for Mutable {}
impl State for Snapshot {}
impl State for Fixed {}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Mutable {}
    impl Sealed for super::Snapshot {}
    impl Sealed for super::Fixed {}
}

// =============================================================================
// Kind (decision #4: open enum + Custom prefix branch)
// =============================================================================

/// Reserved prefix for first-party `Custom` kinds.
///
/// See the Custom prefix branch memory: only
/// `creo:` is reserved. Construction of [`Kind::Custom`] with this prefix from
/// non-first-party code paths must be rejected at the API boundary.
pub const RESERVED_CUSTOM_PREFIX: &str = "creo:";

/// Classification of a record's payload semantics. Open enum so first-party
/// kinds can be promoted from [`Self::Custom`] without breaking callers
/// (decision #4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Append-only event stream entry.
    Log,
    /// Frozen document snapshot.
    Fix,
    /// Structured tabular dataset.
    Dataset,
    /// Binary blob whose body is stored in a separate engine (decision #8).
    Asset,
    /// User-defined kind. The string MUST NOT use the [`RESERVED_CUSTOM_PREFIX`]
    /// outside first-party contexts; this is enforced at the API boundary,
    /// not at type construction.
    Custom(String),
}

// =============================================================================
// Hash (decision #3: content addressing for Asset / Fixed)
// =============================================================================

/// SHA-256 content hash newtype.
///
/// Required on `Record<Fixed>` whose [`Kind`] is [`Kind::Asset`] (decision #6).
/// Optional elsewhere; the value is supplied by the caller of [`Record::fix`]
/// (the fossilization metaphor — minerals are injected from outside).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sha256Hash(pub [u8; 32]);

impl std::fmt::Display for Sha256Hash {
    /// Lower-case hex encoding so the value can be embedded in storage keys
    /// like `/fixed/<sha256>` without pulling in a hex crate.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

// =============================================================================
// Body (decision #8: asset is on a separate storage engine)
// =============================================================================

/// Storage location of a record's payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Payload stored inline in the SurrealDB record.
    /// Used by [`Kind::Log`] / [`Kind::Fix`] / [`Kind::Dataset`].
    Inline(Vec<u8>),
    /// Payload stored in the asset blob storage backend.
    /// Used by [`Kind::Asset`]. The `key` follows the migratory-bird convention
    /// (Phase 1.5 decision): fluid while [`Mutable`] / [`Snapshot`] (storage
    /// layer's responsibility), rewritten to `/fixed/<sha256>` at
    /// [`Record::fix`].
    BlobRef {
        /// Storage key.
        key: String,
        /// Payload size in bytes.
        size: u64,
    },
}

// =============================================================================
// Version (Phase 1.5: snapshot / annual rings)
// =============================================================================

/// One entry in the version chain — an `(id, body)` pair.
///
/// Storing the body alongside the id preserves the full snapshot history
/// through `fix()` (annual-rings analogue, Phase 1.5). For [`Body::Inline`]
/// the bytes are duplicated per version. For [`Body::BlobRef`] only the key
/// and size are kept, so the cost is constant regardless of blob size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// UUID v7 (decision #10).
    pub id: VersionId,
    /// Snapshot of the body at this version.
    pub body: Body,
}

/// Identifier of a single immutable version within a record's chain.
///
/// Backed by UUID v7 (decision #10) so version ids are time-sortable and the
/// timestamp is recoverable from the id itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionId(pub Uuid);

// =============================================================================
// Record<S>
// =============================================================================

/// The unit of storage in Object Records, parameterised over its [`State`].
///
/// State transitions (Phase 1.5):
/// - `Record<Mutable>::update(&mut self, body)` — mutate in place
/// - `Record<Mutable>::snapshot(&self) -> Record<Snapshot>` — clone-derive
/// - `Record<Mutable>::fix(self, hash) -> Record<Fixed>` — direct fossilize
/// - `Record<Snapshot>::fix(self, hash) -> Record<Fixed>` — fossilize from
///   snapshot
///
/// `Record<Fixed>` has no mutating methods — attempts to mutate are compile
/// errors.
#[derive(Debug, Clone)]
pub struct Record<S: State> {
    id: Uuid,
    kind: Kind,
    content_hash: Option<Sha256Hash>,
    versions: Vec<Version>,
    _state: PhantomData<S>,
}

impl<S: State> Record<S> {
    /// Returns the record id (UUID v7, stable across the whole chain and
    /// across snapshots taken from the same source).
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the kind of this record.
    #[must_use]
    pub fn kind(&self) -> &Kind {
        &self.kind
    }

    /// Returns the body of the latest version.
    ///
    /// # Panics
    ///
    /// Panics if `versions` is empty, which is a broken invariant — `new`
    /// always seeds at least one version and `update` only appends.
    #[must_use]
    pub fn body(&self) -> &Body {
        &self
            .versions
            .last()
            .expect("invariant: versions is never empty after `new`")
            .body
    }

    /// Returns the content hash if one has been computed.
    ///
    /// Always `Some` for `Record<Fixed>` whose kind is [`Kind::Asset`]
    /// (decision #6). For `Record<Mutable>` / `Record<Snapshot>` always
    /// `None`.
    #[must_use]
    pub fn content_hash(&self) -> Option<&Sha256Hash> {
        self.content_hash.as_ref()
    }

    /// Returns the full version chain, oldest first, with body snapshots
    /// (Phase 1.5 annual-rings).
    #[must_use]
    pub fn versions(&self) -> &[Version] {
        &self.versions
    }

    /// Returns the immediate predecessor of the current version, if any.
    ///
    /// Derived from [`Self::versions`] — a record with only one version has
    /// no predecessor.
    #[must_use]
    pub fn previous_version(&self) -> Option<&VersionId> {
        let len = self.versions.len();
        if len < 2 {
            None
        } else {
            Some(&self.versions[len - 2].id)
        }
    }
}

// =============================================================================
// Fossilization helper (shared by Mutable::fix and Snapshot::fix)
// =============================================================================

/// Performs the fossilization rite (Phase 1.5):
/// - sets `content_hash` to the supplied digest (decision #6)
/// - rewrites trailing `BlobRef.key` to `/fixed/<sha256>` (decision #11,
///   migratory-bird model)
/// - preserves all prior version snapshots (annual-rings)
/// - emits a `Record<Fixed>` (type transition, decision #5)
fn fossilize(
    id: Uuid,
    kind: Kind,
    mut versions: Vec<Version>,
    hash: Sha256Hash,
) -> Record<Fixed> {
    if let Some(last) = versions.last_mut() {
        let stale = std::mem::replace(&mut last.body, Body::Inline(Vec::new()));
        last.body = match stale {
            Body::Inline(b) => Body::Inline(b),
            Body::BlobRef { size, .. } => Body::BlobRef {
                key: format!("/fixed/{hash}"),
                size,
            },
        };
    }

    Record {
        id,
        kind,
        content_hash: Some(hash),
        versions,
        _state: PhantomData,
    }
}

impl Record<Mutable> {
    /// Creates a fresh `Mutable` record with a single-entry version chain.
    ///
    /// The record id and the initial [`VersionId`] are both UUID v7 (decision
    /// #10), so they sort by creation time. `content_hash` starts as `None`;
    /// it is populated only by [`Record::fix`] from a caller-supplied digest
    /// (decision #6 + Phase 1.5 fossilization metaphor).
    #[must_use]
    pub fn new(kind: Kind, body: Body) -> Self {
        let id = Uuid::now_v7();
        let initial = Version {
            id: VersionId(Uuid::now_v7()),
            body,
        };
        Self {
            id,
            kind,
            content_hash: None,
            versions: vec![initial],
            _state: PhantomData,
        }
    }

    /// Reconstructs a `Record<Mutable>` from its component parts.
    ///
    /// Inverse of the (id, kind, versions) read accessors. Exists only to
    /// support persistence layers (e.g., `objectrecords-db`); production
    /// code should reach `Record<Mutable>` via [`Self::new`] / [`Self::update`].
    /// `content_hash` is forced to `None` here, encoding decision #6 directly
    /// in the signature: a Mutable record has no fossilization digest yet.
    ///
    /// # Panics (debug only)
    ///
    /// Panics in debug builds if `versions` is empty — the same invariant as
    /// [`Self::new`] / [`Self::update`].
    #[must_use]
    pub fn from_parts(id: Uuid, kind: Kind, versions: Vec<Version>) -> Self {
        debug_assert!(
            !versions.is_empty(),
            "Record<Mutable>::from_parts: versions chain must be non-empty",
        );
        Self {
            id,
            kind,
            content_hash: None,
            versions,
            _state: PhantomData,
        }
    }

    /// Appends a new [`Version`] (with its own UUID v7) to the chain
    /// (decision #1). The previous body is retained as a snapshot in the
    /// chain (Phase 1.5 annual-rings) — `update` does not lose history.
    pub fn update(&mut self, body: Body) {
        let next = Version {
            id: VersionId(Uuid::now_v7()),
            body,
        };
        self.versions.push(next);
    }

    /// Camera-shutter metaphor (Phase 1.5): produce an independent
    /// [`Snapshot`] copy by cloning the current chain. The Mutable record
    /// continues to evolve afterwards.
    ///
    /// Use cases: preview / draft display / staging / parallel review while
    /// edits continue on the source record.
    #[must_use]
    pub fn snapshot(&self) -> Record<Snapshot> {
        Record {
            id: self.id,
            kind: self.kind.clone(),
            content_hash: None,
            versions: self.versions.clone(),
            _state: PhantomData,
        }
    }

    /// Direct fossilization from `Mutable` to `Fixed`. Skips the [`Snapshot`]
    /// stage; useful when the caller already has the digest and does not
    /// need a review step.
    ///
    /// See module-level docs and [`fossilize`] for the side effects.
    #[must_use]
    pub fn fix(self, hash: Sha256Hash) -> Record<Fixed> {
        fossilize(self.id, self.kind, self.versions, hash)
    }
}

impl Record<Snapshot> {
    /// Reconstructs a `Record<Snapshot>` from its component parts.
    ///
    /// Inverse of the read accessors, for persistence layer use only — same
    /// constraints as [`Record::<Mutable>::from_parts`]. `content_hash` is
    /// forced to `None` because a Snapshot is pre-fossilization (decision #6).
    ///
    /// # Panics (debug only)
    ///
    /// Panics in debug builds if `versions` is empty.
    #[must_use]
    pub fn from_parts(id: Uuid, kind: Kind, versions: Vec<Version>) -> Self {
        debug_assert!(
            !versions.is_empty(),
            "Record<Snapshot>::from_parts: versions chain must be non-empty",
        );
        Self {
            id,
            kind,
            content_hash: None,
            versions,
            _state: PhantomData,
        }
    }

    /// Fossilize a snapshot into a fixed record. Same semantics as
    /// [`Record::<Mutable>::fix`] — see [`fossilize`].
    #[must_use]
    pub fn fix(self, hash: Sha256Hash) -> Record<Fixed> {
        fossilize(self.id, self.kind, self.versions, hash)
    }
}

impl Record<Fixed> {
    /// Reconstructs a `Record<Fixed>` from its component parts.
    ///
    /// Inverse of the read accessors. Unlike [`Record::<Mutable>::from_parts`]
    /// and [`Record::<Snapshot>::from_parts`], `content_hash` is **required**
    /// here — decision #6 mandates that every Fixed record carries its
    /// fossilization digest, and this is encoded in the signature.
    ///
    /// The caller is responsible for keeping the trailing
    /// [`Body::BlobRef::key`] (if any) in sync with the supplied hash
    /// (`/fixed/<hash>` per decision #11). Production code reaches
    /// `Record<Fixed>` only via [`Record::<Mutable>::fix`] /
    /// [`Record::<Snapshot>::fix`], which handle this rewrite.
    ///
    /// # Panics (debug only)
    ///
    /// Panics in debug builds if `versions` is empty.
    #[must_use]
    pub fn from_parts(
        id: Uuid,
        kind: Kind,
        content_hash: Sha256Hash,
        versions: Vec<Version>,
    ) -> Self {
        debug_assert!(
            !versions.is_empty(),
            "Record<Fixed>::from_parts: versions chain must be non-empty",
        );
        Self {
            id,
            kind,
            content_hash: Some(content_hash),
            versions,
            _state: PhantomData,
        }
    }
}

// `Record<Fixed>` intentionally has no mutating methods.
// `Record<Snapshot>` intentionally has no `update` method.
// `compile_fail` trybuild tests will be added when storage / api crates land.

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Phase 1.0 (already green) ---------------------------------------

    #[test]
    fn mutable_record_can_be_created() {
        let record = Record::<Mutable>::new(Kind::Log, Body::Inline(b"hello".to_vec()));
        assert_eq!(record.versions().len(), 1);
        assert!(record.previous_version().is_none());
    }

    #[test]
    fn update_appends_to_version_chain() {
        let mut record = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![1]));
        let original_id = record.id();
        record.update(Body::Inline(vec![2]));

        assert_eq!(record.id(), original_id, "record id is stable across versions");
        assert_eq!(record.versions().len(), 2);
    }

    #[test]
    fn uuid_v7_yields_monotonic_sort_order() {
        let a = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![]));
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![]));
        assert!(a.id() < b.id(), "later record must sort after earlier one");
    }

    #[test]
    fn kind_custom_rejects_non_creo_first_party_prefix() {
        assert_eq!(RESERVED_CUSTOM_PREFIX, "creo:");
        let _first_party_ok = Kind::Custom("creo:internal".into());
        let _user_ok = Kind::Custom("user:my-thing".into());
    }

    #[test]
    fn mutable_asset_content_hash_is_optional() {
        let record = Record::<Mutable>::new(
            Kind::Asset,
            Body::BlobRef {
                key: "/mutable/abc".into(),
                size: 4,
            },
        );
        assert!(record.content_hash().is_none());
    }

    #[test]
    fn previous_version_links_to_chain_tail() {
        let mut record = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![1]));
        record.update(Body::Inline(vec![2]));

        let versions = record.versions();
        let previous = record
            .previous_version()
            .expect("must have a previous after update");
        assert_eq!(previous, &versions[versions.len() - 2].id);
    }

    // ----- Phase 1.5 (fossilization) ---------------------------------------

    #[test]
    fn fix_transitions_to_fixed_state() {
        let record = Record::<Mutable>::new(Kind::Fix, Body::Inline(b"frozen".to_vec()));
        let _fixed: Record<Fixed> = record.fix(Sha256Hash([0u8; 32]));
        // The compiler enforces that `_fixed.update(...)` would not type-check.
    }

    #[test]
    fn fixed_asset_requires_content_hash() {
        let record = Record::<Mutable>::new(
            Kind::Asset,
            Body::BlobRef {
                key: "/mutable/abc".into(),
                size: 4,
            },
        );
        let fixed = record.fix(Sha256Hash([0xAB; 32]));
        assert!(
            fixed.content_hash().is_some(),
            "Fixed Asset must have a content hash (decision #6)",
        );
    }

    #[test]
    fn fixed_storage_key_uses_content_hash() {
        let record = Record::<Mutable>::new(
            Kind::Asset,
            Body::BlobRef {
                key: "/mutable/will-be-overwritten".into(),
                size: 0,
            },
        );
        let hash = Sha256Hash([0xCD; 32]);
        let fixed = record.fix(hash);
        match fixed.body() {
            Body::BlobRef { key, .. } => {
                assert!(key.starts_with("/fixed/"));
                assert_eq!(key, &format!("/fixed/{}", "cd".repeat(32)));
            }
            Body::Inline(_) => panic!("asset body must be BlobRef"),
        }
    }

    // ----- Phase 1.5 (annual rings: snapshot history) ----------------------

    #[test]
    fn versions_preserve_full_body_history() {
        let mut record = Record::<Mutable>::new(Kind::Log, Body::Inline(b"v1".to_vec()));
        record.update(Body::Inline(b"v2".to_vec()));
        record.update(Body::Inline(b"v3".to_vec()));

        let versions = record.versions();
        assert_eq!(versions.len(), 3);
        match &versions[0].body {
            Body::Inline(b) => assert_eq!(b.as_slice(), b"v1"),
            Body::BlobRef { .. } => panic!("expected inline"),
        }
        match &versions[1].body {
            Body::Inline(b) => assert_eq!(b.as_slice(), b"v2"),
            Body::BlobRef { .. } => panic!("expected inline"),
        }
        match &versions[2].body {
            Body::Inline(b) => assert_eq!(b.as_slice(), b"v3"),
            Body::BlobRef { .. } => panic!("expected inline"),
        }
    }

    #[test]
    fn fix_preserves_full_version_history() {
        let mut record = Record::<Mutable>::new(Kind::Log, Body::Inline(b"v1".to_vec()));
        record.update(Body::Inline(b"v2".to_vec()));

        let fixed = record.fix(Sha256Hash([0u8; 32]));
        assert_eq!(fixed.versions().len(), 2);

        match &fixed.versions()[0].body {
            Body::Inline(b) => assert_eq!(b.as_slice(), b"v1"),
            Body::BlobRef { .. } => panic!("expected inline"),
        }
        match &fixed.versions()[1].body {
            Body::Inline(b) => assert_eq!(b.as_slice(), b"v2"),
            Body::BlobRef { .. } => panic!("expected inline"),
        }
    }

    // ----- Phase 1.5 (Snapshot state: camera-shutter) ----------------------

    /// Phase 1.5: snapshot(&self) yields an independent immutable copy.
    #[test]
    fn snapshot_can_be_taken_from_mutable() {
        let mut record = Record::<Mutable>::new(Kind::Log, Body::Inline(b"v1".to_vec()));
        record.update(Body::Inline(b"v2".to_vec()));

        let snap: Record<Snapshot> = record.snapshot();
        assert_eq!(snap.versions().len(), 2);
        assert_eq!(snap.id(), record.id(), "snapshot shares id with source mutable");
    }

    /// Camera-shutter: taking a snapshot does not stop the Mutable evolution.
    #[test]
    fn mutable_continues_evolving_after_snapshot() {
        let mut record = Record::<Mutable>::new(Kind::Log, Body::Inline(b"v1".to_vec()));
        let snap = record.snapshot();
        record.update(Body::Inline(b"v2".to_vec()));

        assert_eq!(snap.versions().len(), 1, "snapshot frozen at v1");
        assert_eq!(record.versions().len(), 2, "mutable advanced to v2");
    }

    /// Snapshot can be fossilized — same end shape as direct Mutable -> Fixed.
    #[test]
    fn snapshot_can_be_fossilized() {
        let record = Record::<Mutable>::new(Kind::Log, Body::Inline(b"draft".to_vec()));
        let snap = record.snapshot();
        let fixed: Record<Fixed> = snap.fix(Sha256Hash([0u8; 32]));

        assert_eq!(fixed.versions().len(), 1);
        assert!(fixed.content_hash().is_some());
    }

    /// Snapshot of an Asset preserves the BlobRef as-is (migratory-bird:
    /// fluid until fix). Only fix() rewrites the key.
    #[test]
    fn snapshot_of_asset_preserves_mutable_blob_key() {
        let record = Record::<Mutable>::new(
            Kind::Asset,
            Body::BlobRef {
                key: "/mutable/draft-abc".into(),
                size: 12,
            },
        );
        let snap = record.snapshot();
        match snap.body() {
            Body::BlobRef { key, size } => {
                assert_eq!(key, "/mutable/draft-abc");
                assert_eq!(*size, 12);
            }
            Body::Inline(_) => panic!("asset body must be BlobRef"),
        }
    }
}
