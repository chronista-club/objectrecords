//! Phase 3.0 — Small tier round-trip tests for the persistence layer.
//!
//! These tests are entirely DB-free; they exercise only [`PersistedRow`]
//! serialisation invariants and the `From<&Record<S>>` /
//! `TryFrom<PersistedRow>` conversion logic in
//! [`objectrecords_db::convert`].
//!
//! The Medium tier (live SurrealDB CRUD against `surreal-local`) ships
//! in Phase 3.1 — see Sub-Q4 default in
//!.

use objectrecords_core::{Body, Fixed, Kind, Mutable, Record, Sha256Hash, Snapshot};
use objectrecords_db::{
    DbError, PersistedAttribution, PersistedBody, PersistedRecord, PersistedRow, PersistedState,
    PersistedVersion, record_to_persisted_row,
};
use uuid::Uuid;

/// Synthetic creator used by every Phase 3.0 / 3.1 / 3.2 round-trip
/// test (Phase 3.2 renamed from the previous `TEST_OWNER` constant to
/// align with the Paradigm Ⅱ shift — attribution travels on a graph
/// edge, not as a record column). Production creators take the form
/// `creo_user:<sub-claim>`; the `:test-suite` suffix marks records
/// that originated from the test harness so they can be filtered out
/// of any audit query.
const TEST_CREATOR: &str = "creo_user:test-suite";

// =============================================================================
// Helpers
// =============================================================================

/// Convenience builder for a sample SHA-256 — the bytes are simply
/// `[0, 1, 2, ..., 31]`, which renders as
/// `"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"`.
fn sample_hash() -> Sha256Hash {
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = i as u8;
    }
    Sha256Hash(bytes)
}

/// Asserts that the conversion `Record<S> -> PersistedRow -> Record<S>`
/// preserves the semantically meaningful fields. We can't compare
/// `Record<S>` directly because it has `PhantomData` and private fields;
/// instead we project to its accessors.
fn assert_records_equivalent<S: objectrecords_core::State>(left: &Record<S>, right: &Record<S>) {
    assert_eq!(left.id(), right.id(), "record id");
    assert_eq!(left.kind(), right.kind(), "record kind");
    assert_eq!(left.content_hash(), right.content_hash(), "content_hash");
    assert_eq!(
        left.versions().len(),
        right.versions().len(),
        "version chain length",
    );
    for (l, r) in left.versions().iter().zip(right.versions().iter()) {
        assert_eq!(l.id, r.id, "version id");
        assert_eq!(l.body, r.body, "version body");
    }
}

// =============================================================================
// Forward + reverse round-trip
// =============================================================================

#[test]
fn round_trip_mutable_single_version() {
    let original = Record::<Mutable>::new(Kind::Log, Body::Inline(b"hello".to_vec()));

    let row: PersistedRow = record_to_persisted_row(&original, TEST_CREATOR.to_string());
    assert_eq!(row.record.state, PersistedState::Mutable);
    assert_eq!(row.record.kind, "log");
    assert_eq!(row.record.content_hash, None);
    assert_eq!(row.versions.len(), 1);
    assert_eq!(row.versions[0].record_id, row.record.id);

    let rebuilt = Record::<Mutable>::try_from(row).unwrap();
    assert_records_equivalent(&original, &rebuilt);
}

#[test]
fn round_trip_mutable_multi_version_chain() {
    let mut original = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![1]));
    original.update(Body::Inline(vec![2]));
    original.update(Body::Inline(vec![3]));

    let row: PersistedRow = record_to_persisted_row(&original, TEST_CREATOR.to_string());
    assert_eq!(row.versions.len(), 3, "annual rings preserved (decision #16)");

    // All version rows must FK back to the same record id (decision #26).
    for v in &row.versions {
        assert_eq!(v.record_id, row.record.id);
    }

    let rebuilt = Record::<Mutable>::try_from(row).unwrap();
    assert_records_equivalent(&original, &rebuilt);
}

#[test]
fn round_trip_snapshot_preserves_history() {
    let mut source = Record::<Mutable>::new(Kind::Dataset, Body::Inline(b"v1".to_vec()));
    source.update(Body::Inline(b"v2".to_vec()));
    let snapshot = source.snapshot();

    let row: PersistedRow = record_to_persisted_row(&snapshot, TEST_CREATOR.to_string());
    assert_eq!(row.record.state, PersistedState::Snapshot);
    assert_eq!(row.record.kind, "dataset");

    let rebuilt = Record::<Snapshot>::try_from(row).unwrap();
    assert_records_equivalent(&snapshot, &rebuilt);
}

#[test]
fn round_trip_fixed_preserves_content_hash() {
    let mutable = Record::<Mutable>::new(Kind::Fix, Body::Inline(b"once frozen".to_vec()));
    let hash = sample_hash();
    let original = mutable.fix(hash.clone());

    let row: PersistedRow = record_to_persisted_row(&original, TEST_CREATOR.to_string());
    assert_eq!(row.record.state, PersistedState::Fixed);
    assert_eq!(
        row.record.content_hash.as_deref(),
        Some("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"),
        "content_hash hex matches Sha256Hash::Display lowercase form",
    );

    let rebuilt = Record::<Fixed>::try_from(row).unwrap();
    assert_records_equivalent(&original, &rebuilt);
    assert_eq!(rebuilt.content_hash(), Some(&hash));
}

#[test]
fn round_trip_asset_blob_ref_body() {
    let original = Record::<Mutable>::new(
        Kind::Asset,
        Body::BlobRef {
            key: "/uploads/abc".to_string(),
            size: 4096,
        },
    );

    let row: PersistedRow = record_to_persisted_row(&original, TEST_CREATOR.to_string());
    assert_eq!(row.record.kind, "asset");
    match &row.versions[0].body {
        PersistedBody::BlobRef { key, size } => {
            assert_eq!(key, "/uploads/abc");
            assert_eq!(*size, 4096);
        }
        other => panic!("expected BlobRef, got {other:?}"),
    }

    let rebuilt = Record::<Mutable>::try_from(row).unwrap();
    assert_records_equivalent(&original, &rebuilt);
}

#[test]
fn round_trip_custom_kind_with_creo_prefix() {
    let original = Record::<Mutable>::new(
        Kind::Custom("creo:bikeboy.session".to_string()),
        Body::Inline(b"{}".to_vec()),
    );

    let row: PersistedRow = record_to_persisted_row(&original, TEST_CREATOR.to_string());
    assert_eq!(row.record.kind, "creo:bikeboy.session");

    let rebuilt = Record::<Mutable>::try_from(row).unwrap();
    assert_records_equivalent(&original, &rebuilt);
}

#[test]
fn fixed_record_storage_key_uses_fixed_prefix() {
    // After fix(), core rewrites trailing BlobRef.key to /fixed/<hash>
    // (decision #15, migratory-bird model). Persistence must preserve
    // this rewritten key verbatim.
    let mutable = Record::<Mutable>::new(
        Kind::Asset,
        Body::BlobRef {
            key: "/uploads/staging/xyz".to_string(),
            size: 10,
        },
    );
    let hash = sample_hash();
    let fixed = mutable.fix(hash);

    let row: PersistedRow = record_to_persisted_row(&fixed, TEST_CREATOR.to_string());
    let last = row.versions.last().expect("at least one version");
    match &last.body {
        PersistedBody::BlobRef { key, .. } => {
            assert!(
                key.starts_with("/fixed/"),
                "Fixed asset key must be /fixed/<sha256>, got: {key}",
            );
        }
        other => panic!("expected BlobRef, got {other:?}"),
    }
}

// =============================================================================
// Documented lossy edge: Custom name colliding with a built-in
// =============================================================================

#[test]
fn custom_kind_colliding_with_builtin_round_trips_as_builtin() {
    // Documented contract: Kind::Custom("log") forward-converts to the
    // string "log" then reverse-converts to Kind::Log — i.e., the round
    // trip is *lossy* if a caller violates the `creo:` prefix
    // convention. This test pins that behaviour so any future change
    // (e.g., adding a discriminator field) is a deliberate choice.
    let original = Record::<Mutable>::new(
        Kind::Custom("log".to_string()),
        Body::Inline(b"".to_vec()),
    );

    let row: PersistedRow = record_to_persisted_row(&original, TEST_CREATOR.to_string());
    let rebuilt = Record::<Mutable>::try_from(row).unwrap();
    assert_eq!(
        rebuilt.kind(),
        &Kind::Log,
        "Custom collision with built-in is intentionally lossy (see convert.rs docs)",
    );
}

// =============================================================================
// Error paths
// =============================================================================

/// Builds a minimal `PersistedRow` with one inline version, for use as a
/// scaffold in the negative tests below.
fn scaffold_row(state: PersistedState, content_hash: Option<String>) -> PersistedRow {
    let id = Uuid::now_v7();
    let version_id = Uuid::now_v7();
    let now = chrono::Utc::now();
    PersistedRow {
        record: PersistedRecord {
            id,
            state,
            kind: "log".to_string(),
            content_hash,
            created_at: now,
            updated_at: now,
        },
        versions: vec![PersistedVersion {
            id: version_id,
            record_id: id,
            body: PersistedBody::Inline { bytes: vec![1] },
            created_at: now,
        }],
        attribution: PersistedAttribution {
            creator: TEST_CREATOR.to_string(),
            at: now,
        },
    }
}

#[test]
fn err_state_mismatch_when_loading_fixed_row_as_mutable() {
    let row = scaffold_row(PersistedState::Fixed, Some("0".repeat(64)));
    let err = Record::<Mutable>::try_from(row).unwrap_err();
    assert!(
        matches!(&err, DbError::StateMismatch { actual, requested }
            if actual == "fixed" && *requested == "mutable"),
        "expected StateMismatch, got {err:?}",
    );
}

#[test]
fn err_unexpected_content_hash_on_mutable_row() {
    let row = scaffold_row(PersistedState::Mutable, Some("0".repeat(64)));
    let err = Record::<Mutable>::try_from(row).unwrap_err();
    assert!(
        matches!(err, DbError::UnexpectedContentHash { state: "Mutable", .. }),
        "expected UnexpectedContentHash for Mutable, got {err:?}",
    );
}

#[test]
fn err_unexpected_content_hash_on_snapshot_row() {
    let row = scaffold_row(PersistedState::Snapshot, Some("0".repeat(64)));
    let err = Record::<Snapshot>::try_from(row).unwrap_err();
    assert!(
        matches!(err, DbError::UnexpectedContentHash { state: "Snapshot", .. }),
        "expected UnexpectedContentHash for Snapshot, got {err:?}",
    );
}

#[test]
fn err_missing_content_hash_on_fixed_row() {
    let row = scaffold_row(PersistedState::Fixed, None);
    let err = Record::<Fixed>::try_from(row).unwrap_err();
    assert!(
        matches!(err, DbError::MissingContentHash(_)),
        "expected MissingContentHash, got {err:?}",
    );
}

#[test]
fn err_empty_version_chain() {
    let mut row = scaffold_row(PersistedState::Mutable, None);
    row.versions.clear();
    let err = Record::<Mutable>::try_from(row).unwrap_err();
    assert!(
        matches!(err, DbError::EmptyVersionChain(_)),
        "expected EmptyVersionChain, got {err:?}",
    );
}

#[test]
fn err_malformed_hash_too_short() {
    let row = scaffold_row(PersistedState::Fixed, Some("abc".to_string()));
    let err = Record::<Fixed>::try_from(row).unwrap_err();
    assert!(
        matches!(err, DbError::MalformedHash(_)),
        "expected MalformedHash for short hash, got {err:?}",
    );
}

#[test]
fn err_malformed_hash_uppercase_hex_rejected() {
    // Sha256Hash::Display emits lowercase; we reject uppercase to keep
    // round-trip byte-equal.
    let upper = "A".repeat(64);
    let row = scaffold_row(PersistedState::Fixed, Some(upper));
    let err = Record::<Fixed>::try_from(row).unwrap_err();
    assert!(
        matches!(err, DbError::MalformedHash(_)),
        "expected MalformedHash for uppercase hex, got {err:?}",
    );
}

#[test]
fn err_malformed_hash_non_hex_chars() {
    // 64 chars but contains a non-hex character.
    let bad = format!("{}{}", "0".repeat(63), "z");
    let row = scaffold_row(PersistedState::Fixed, Some(bad));
    let err = Record::<Fixed>::try_from(row).unwrap_err();
    assert!(
        matches!(err, DbError::MalformedHash(_)),
        "expected MalformedHash for non-hex char, got {err:?}",
    );
}
