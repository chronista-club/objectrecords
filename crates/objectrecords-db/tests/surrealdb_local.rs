//! Live integration tests for [`SurrealRecordRepository`] against a
//! running SurrealDB instance (default target: the `surreal-local`
//! dev container at `ws://127.0.0.1:12000`, decision #25).
//!
//! # Running
//!
//! Gated by `OBJECTRECORDS_SURREAL_TEST_ENDPOINT` so the default
//! `cargo test --workspace` run stays green even without a live
//! SurrealDB. To enable:
//!
//! ```ignore
//! export OBJECTRECORDS_SURREAL_TEST_ENDPOINT=ws://127.0.0.1:12000
//! cargo test -p objectrecords-db --test surrealdb_local
//! ```
//!
//! Optional overrides — defaults match Sub-Q4 of the Phase 3.0 kickoff
//! memory:
//!
//! - `OBJECTRECORDS_SURREAL_TEST_NAMESPACE` (default
//!   `objectrecords_test`)
//! - `OBJECTRECORDS_SURREAL_TEST_DATABASE` (default `main`)
//! - `OBJECTRECORDS_SURREAL_TEST_USERNAME` (default `admin`)
//! - `OBJECTRECORDS_SURREAL_TEST_PASSWORD` (default `admin-local-dev`)
//!
//! Each test uses a fresh UUID v7 as its record id, so concurrent runs
//! never collide. The `objectrecords_test` namespace can be wiped
//! manually between runs (`surreal sql ... 'REMOVE TABLE record;
//! REMOVE TABLE version; REMOVE TABLE created_by; REMOVE TABLE
//! snapshot_of;'`) — same operational discipline as the storage
//! crate's `objectrecords-test` SeaweedFS bucket.

use std::env;

use objectrecords_core::{Body, Kind, Mutable, Record, Sha256Hash};
use objectrecords_db::{
    AnyRecord, DbError, ObjectRecordsDb, ObjectRecordsDbConfig, RecordRepository,
    SurrealRecordRepository,
};
use uuid::Uuid;

// =============================================================================
// Setup
// =============================================================================

/// Builds a [`SurrealRecordRepository`] from environment variables, or
/// returns `None` if `OBJECTRECORDS_SURREAL_TEST_ENDPOINT` is unset.
async fn setup_repo() -> Option<SurrealRecordRepository> {
    let endpoint = env::var("OBJECTRECORDS_SURREAL_TEST_ENDPOINT").ok()?;
    let namespace = env::var("OBJECTRECORDS_SURREAL_TEST_NAMESPACE")
        .unwrap_or_else(|_| "objectrecords_test".to_string());
    let database = env::var("OBJECTRECORDS_SURREAL_TEST_DATABASE")
        .unwrap_or_else(|_| "main".to_string());
    let username = env::var("OBJECTRECORDS_SURREAL_TEST_USERNAME")
        .unwrap_or_else(|_| "admin".to_string());
    let password = env::var("OBJECTRECORDS_SURREAL_TEST_PASSWORD")
        .unwrap_or_else(|_| "admin-local-dev".to_string());

    let config = ObjectRecordsDbConfig {
        endpoint,
        namespace,
        database,
        username,
        password,
    };
    let db = ObjectRecordsDb::connect(config)
        .await
        .expect("connect to surreal-local");
    Some(SurrealRecordRepository::new(db))
}

/// Synthetic creator. UUID v4 keeps each run independent so multiple
/// concurrent test runs never share attribution edges.
fn unique_creator() -> String {
    format!("creo_user:test-{}", Uuid::new_v4())
}

fn skip(test: &str) {
    eprintln!("[skip] {test}: OBJECTRECORDS_SURREAL_TEST_ENDPOINT not set");
}

fn sample_hash() -> Sha256Hash {
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = i as u8;
    }
    Sha256Hash(bytes)
}

// =============================================================================
// Round-trip CRUD (Paradigm Ⅱ — find returns RecordWithAttribution)
// =============================================================================

#[tokio::test]
async fn mutable_save_find_round_trip() {
    let Some(repo) = setup_repo().await else {
        skip("mutable_save_find_round_trip");
        return;
    };
    let creator = unique_creator();

    let original = Record::<Mutable>::new(Kind::Log, Body::Inline(b"phase-3.2".to_vec()));
    let id = original.id();
    repo.save(&original, &creator).await.unwrap();

    let rwa = repo.find(id).await.unwrap().expect("record present");
    // Paradigm Ⅱ — attribution travels alongside the typed record.
    assert_eq!(rwa.attribution.creator, creator);
    match rwa.record {
        AnyRecord::Mutable(rebuilt) => {
            assert_eq!(rebuilt.id(), original.id());
            assert_eq!(rebuilt.kind(), original.kind());
            assert_eq!(rebuilt.versions().len(), 1);
            assert_eq!(rebuilt.versions()[0].body, original.versions()[0].body);
        }
        other => panic!("expected Mutable variant, got {other:?}"),
    }

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn fixed_save_find_preserves_content_hash() {
    let Some(repo) = setup_repo().await else {
        skip("fixed_save_find_preserves_content_hash");
        return;
    };
    let creator = unique_creator();

    let mutable = Record::<Mutable>::new(Kind::Fix, Body::Inline(b"frozen".to_vec()));
    let hash = sample_hash();
    let original = mutable.fix(hash.clone());
    let id = original.id();
    repo.save(&original, &creator).await.unwrap();

    let rwa = repo.find(id).await.unwrap().expect("record present");
    assert_eq!(rwa.attribution.creator, creator);
    match rwa.record {
        AnyRecord::Fixed(rebuilt) => {
            assert_eq!(rebuilt.content_hash(), Some(&hash));
        }
        other => panic!("expected Fixed variant, got {other:?}"),
    }

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn save_twice_returns_already_exists() {
    let Some(repo) = setup_repo().await else {
        skip("save_twice_returns_already_exists");
        return;
    };
    let creator = unique_creator();

    let original = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![1]));
    let id = original.id();
    repo.save(&original, &creator).await.unwrap();

    let err = repo.save(&original, &creator).await.unwrap_err();
    assert!(
        matches!(err, DbError::AlreadyExists(collision) if collision == id),
        "expected AlreadyExists({id}), got {err:?}",
    );

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn find_missing_returns_none() {
    let Some(repo) = setup_repo().await else {
        skip("find_missing_returns_none");
        return;
    };
    let id = Uuid::now_v7();
    let result = repo.find(id).await.unwrap();
    assert!(
        result.is_none(),
        "expected None for unknown id, got Some(...)",
    );
}

// =============================================================================
// add_version + delete
// =============================================================================

#[tokio::test]
async fn add_version_appends_to_chain() {
    let Some(repo) = setup_repo().await else {
        skip("add_version_appends_to_chain");
        return;
    };
    let creator = unique_creator();

    let original = Record::<Mutable>::new(Kind::Log, Body::Inline(b"v1".to_vec()));
    let id = original.id();
    repo.save(&original, &creator).await.unwrap();

    let appended = repo
        .add_version(id, Body::Inline(b"v2".to_vec()))
        .await
        .unwrap();
    assert!(
        matches!(appended.body, Body::Inline(ref v) if v == b"v2"),
        "appended Version body must mirror the input body",
    );

    let rwa = repo.find(id).await.unwrap().expect("record present");
    match rwa.record {
        AnyRecord::Mutable(rebuilt) => {
            assert_eq!(rebuilt.versions().len(), 2);
            assert_eq!(rebuilt.versions()[0].body, Body::Inline(b"v1".to_vec()));
            assert_eq!(rebuilt.versions()[1].body, Body::Inline(b"v2".to_vec()));
            assert_eq!(rebuilt.versions()[1].id, appended.id);
        }
        other => panic!("expected Mutable variant, got {other:?}"),
    }

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn delete_removes_record_versions_and_edges() {
    let Some(repo) = setup_repo().await else {
        skip("delete_removes_record_versions_and_edges");
        return;
    };
    let creator = unique_creator();

    let mut original = Record::<Mutable>::new(Kind::Log, Body::Inline(vec![1]));
    original.update(Body::Inline(vec![2]));
    let id = original.id();
    repo.save(&original, &creator).await.unwrap();

    let removed = repo.delete(id).await.unwrap();
    assert!(removed, "delete must report success on existing id");

    let result = repo.find(id).await.unwrap();
    assert!(
        result.is_none(),
        "find after delete must return None — versions cascaded",
    );

    // Idempotent: a second delete reports false.
    let removed_again = repo.delete(id).await.unwrap();
    assert!(!removed_again, "second delete must report false");
}

// =============================================================================
// Phase 3.2 new — Snapshot footgun guard (decision #37)
// =============================================================================

#[tokio::test]
async fn save_rejects_snapshot_with_explicit_redirect() {
    let Some(repo) = setup_repo().await else {
        skip("save_rejects_snapshot_with_explicit_redirect");
        return;
    };
    let creator = unique_creator();

    let mut source = Record::<Mutable>::new(Kind::Log, Body::Inline(b"src".to_vec()));
    source.update(Body::Inline(b"v2".to_vec()));
    let snap = source.snapshot();

    let err = repo.save(&snap, &creator).await.unwrap_err();
    assert!(
        matches!(err, DbError::SaveSnapshotRequiresExplicitMethod),
        "save<Snapshot> must redirect to save_snapshot, got {err:?}",
    );
}

// =============================================================================
// Phase 3.2 new — save_snapshot allocates fresh DB id (decision #37)
// =============================================================================

#[tokio::test]
async fn save_snapshot_creates_independent_db_id() {
    let Some(repo) = setup_repo().await else {
        skip("save_snapshot_creates_independent_db_id");
        return;
    };
    let creator = unique_creator();

    // Persist source Mutable, then take a snapshot of it. The two
    // share their in-memory id, but save_snapshot must allocate a
    // fresh DB id so both rows can coexist.
    let mut source = Record::<Mutable>::new(Kind::Log, Body::Inline(b"src".to_vec()));
    source.update(Body::Inline(b"src-v2".to_vec()));
    let source_id = source.id();
    repo.save(&source, &creator).await.unwrap();

    let snap = source.snapshot();
    let snapshot_db_id = repo
        .save_snapshot(&snap, &creator)
        .await
        .expect("save_snapshot succeeds");
    assert_ne!(
        snapshot_db_id, source_id,
        "save_snapshot must allocate a fresh DB id",
    );

    // Both rows are findable by their respective ids.
    let src_rwa = repo
        .find(source_id)
        .await
        .unwrap()
        .expect("source still present");
    assert!(matches!(src_rwa.record, AnyRecord::Mutable(_)));

    let snap_rwa = repo
        .find(snapshot_db_id)
        .await
        .unwrap()
        .expect("snapshot row present");
    assert!(matches!(snap_rwa.record, AnyRecord::Snapshot(_)));

    repo.delete(source_id).await.unwrap();
    repo.delete(snapshot_db_id).await.unwrap();
}

// =============================================================================
// Phase 3.2 new — transition_to_fixed (decision #34)
// =============================================================================

#[tokio::test]
async fn transition_to_fixed_promotes_mutable_state() {
    let Some(repo) = setup_repo().await else {
        skip("transition_to_fixed_promotes_mutable_state");
        return;
    };
    let creator = unique_creator();

    // Persist Mutable, then transition it to Fixed in DB.
    let original = Record::<Mutable>::new(
        Kind::Asset,
        Body::BlobRef {
            key: "/uploads/staging/abc".to_string(),
            size: 1024,
        },
    );
    let id = original.id();
    repo.save(&original, &creator).await.unwrap();

    let hash = sample_hash();
    let fixed = repo
        .transition_to_fixed(id, hash.clone())
        .await
        .expect("transition succeeds");
    assert_eq!(fixed.content_hash(), Some(&hash));

    // Re-load and confirm DB state is fixed; trailing BlobRef.key
    // rewritten to /fixed/<hash> per decision #15.
    let rwa = repo.find(id).await.unwrap().expect("record present");
    match rwa.record {
        AnyRecord::Fixed(rebuilt) => {
            assert_eq!(rebuilt.content_hash(), Some(&hash));
            match &rebuilt.versions().last().unwrap().body {
                Body::BlobRef { key, .. } => {
                    assert!(
                        key.starts_with("/fixed/"),
                        "Fixed asset key must be /fixed/<sha256>, got: {key}",
                    );
                }
                other => panic!("expected BlobRef, got {other:?}"),
            }
        }
        other => panic!("expected Fixed variant after transition, got {other:?}"),
    }

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn transition_to_fixed_rejects_already_fixed() {
    let Some(repo) = setup_repo().await else {
        skip("transition_to_fixed_rejects_already_fixed");
        return;
    };
    let creator = unique_creator();

    let mutable = Record::<Mutable>::new(Kind::Fix, Body::Inline(b"once".to_vec()));
    let hash = sample_hash();
    let fixed = mutable.fix(hash.clone());
    let id = fixed.id();
    repo.save(&fixed, &creator).await.unwrap();

    let err = repo.transition_to_fixed(id, hash).await.unwrap_err();
    assert!(
        matches!(err, DbError::StateMismatch { ref actual, .. } if actual == "fixed"),
        "transition on a Fixed row must error, got {err:?}",
    );

    repo.delete(id).await.unwrap();
}

// =============================================================================
// Phase 3.2 new — list_by_creator (decision #36)
// =============================================================================

#[tokio::test]
async fn list_by_creator_returns_only_records_of_that_creator() {
    let Some(repo) = setup_repo().await else {
        skip("list_by_creator_returns_only_records_of_that_creator");
        return;
    };
    let creator_a = unique_creator();
    let creator_b = unique_creator();

    // 2 records by creator_a, 1 by creator_b.
    let r_a1 = Record::<Mutable>::new(Kind::Log, Body::Inline(b"a1".to_vec()));
    let r_a2 = Record::<Mutable>::new(Kind::Log, Body::Inline(b"a2".to_vec()));
    let r_b1 = Record::<Mutable>::new(Kind::Log, Body::Inline(b"b1".to_vec()));
    let id_a1 = r_a1.id();
    let id_a2 = r_a2.id();
    let id_b1 = r_b1.id();
    repo.save(&r_a1, &creator_a).await.unwrap();
    repo.save(&r_a2, &creator_a).await.unwrap();
    repo.save(&r_b1, &creator_b).await.unwrap();

    let a_records = repo
        .list_by_creator(&creator_a)
        .await
        .expect("list_by_creator(a)");
    let b_records = repo
        .list_by_creator(&creator_b)
        .await
        .expect("list_by_creator(b)");

    assert_eq!(
        a_records.len(),
        2,
        "creator_a should see exactly its 2 records, got {}",
        a_records.len(),
    );
    assert_eq!(
        b_records.len(),
        1,
        "creator_b should see exactly its 1 record, got {}",
        b_records.len(),
    );

    // Every returned RecordWithAttribution has matching attribution.
    for rwa in &a_records {
        assert_eq!(rwa.attribution.creator, creator_a);
    }
    for rwa in &b_records {
        assert_eq!(rwa.attribution.creator, creator_b);
    }

    // Cleanup.
    for id in [id_a1, id_a2, id_b1] {
        repo.delete(id).await.unwrap();
    }
}
