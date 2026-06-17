//! Live integration tests for [`S3CompatStorage`] against an S3-compatible
//! backend (default target: SeaweedFS 4.22+ at `localhost:12100`).
//!
//! # Running
//!
//! These tests are **gated by environment variables** so the default
//! `cargo test --workspace` run stays green even when no S3 backend is
//! reachable. To enable, set:
//!
//! ```ignore
//! export OBJECTRECORDS_S3_TEST_ENDPOINT=http://localhost:12100
//! cargo test --workspace --test s3_compat_seaweedfs
//! ```
//!
//! Optional overrides (defaults match the `creo-memories-local-seaweedfs`
//! container — see project CLAUDE.md decision #23):
//!
//! - `OBJECTRECORDS_S3_TEST_BUCKET` (default `objectrecords-test`)
//! - `OBJECTRECORDS_S3_TEST_REGION` (default `us-east-1`)
//! - `OBJECTRECORDS_S3_TEST_ACCESS_KEY` (default `seaweedfs`)
//! - `OBJECTRECORDS_S3_TEST_SECRET_KEY` (default `seaweedfs-local-dev`)
//! - `OBJECTRECORDS_S3_TEST_ALLOW_HTTP` (default `true`)
//!
//! # Bucket setup (one-time)
//!
//! `object_store` does not expose bucket creation. Pre-create the bucket
//! before running the tests:
//!
//! ```sh
//! AWS_ACCESS_KEY_ID=seaweedfs AWS_SECRET_ACCESS_KEY=seaweedfs-local-dev \
//!   aws --endpoint-url http://localhost:12100 s3 mb s3://objectrecords-test
//! ```
//!
//! Each test uses a UUID-prefixed key so concurrent runs do not collide.
//! Garbage objects accumulate in the test bucket — wipe periodically with
//! `aws s3 rm --recursive s3://objectrecords-test/test/`.

use std::env;
use std::time::Duration;

use bytes::Bytes;
use objectrecords_storage::s3_compat::{S3CompatConfig, S3CompatStorage};
use objectrecords_storage::{BlobStorage, BlobStorageError, PutOutcome};
use uuid::Uuid;

/// Builds a [`S3CompatStorage`] from environment variables, or returns
/// `None` if `OBJECTRECORDS_S3_TEST_ENDPOINT` is unset.
///
/// Each integration test calls this and `return`s early on `None`, so the
/// default `cargo test` run (no env, no live backend) leaves the test green
/// without `#[ignore]` boilerplate.
fn setup_storage() -> Option<S3CompatStorage> {
    let endpoint = env::var("OBJECTRECORDS_S3_TEST_ENDPOINT").ok()?;
    let bucket = env::var("OBJECTRECORDS_S3_TEST_BUCKET")
        .unwrap_or_else(|_| "objectrecords-test".to_string());
    let region = env::var("OBJECTRECORDS_S3_TEST_REGION")
        .unwrap_or_else(|_| "us-east-1".to_string());
    let access_key = env::var("OBJECTRECORDS_S3_TEST_ACCESS_KEY")
        .unwrap_or_else(|_| "seaweedfs".to_string());
    let secret_key = env::var("OBJECTRECORDS_S3_TEST_SECRET_KEY")
        .unwrap_or_else(|_| "seaweedfs-local-dev".to_string());
    let allow_http = env::var("OBJECTRECORDS_S3_TEST_ALLOW_HTTP")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true);

    let config = S3CompatConfig {
        endpoint,
        region,
        bucket,
        access_key,
        secret_key,
        allow_http,
    };

    Some(S3CompatStorage::from_config(config).expect("invalid S3 config"))
}

/// Returns a fresh, collision-free key for a single test. The `test/` prefix
/// keeps the bucket organised so periodic cleanup is simple.
fn unique_key() -> String {
    format!("test/{}/blob", Uuid::new_v4())
}

/// Logs the skip reason in a format `cargo test --nocapture` will show.
fn skip(test: &str) {
    eprintln!("[skip] {test}: OBJECTRECORDS_S3_TEST_ENDPOINT not set");
}

#[tokio::test]
async fn s3_put_then_get_returns_same_blob() {
    let Some(storage) = setup_storage() else {
        skip("put_then_get");
        return;
    };
    let key = unique_key();
    let payload = Bytes::from_static(b"hello world");
    storage.put(&key, payload.clone()).await.unwrap();

    let got = storage.get(&key).await.unwrap();
    assert_eq!(got, payload);
}

#[tokio::test]
async fn s3_get_missing_returns_not_found() {
    let Some(storage) = setup_storage() else {
        skip("get_missing");
        return;
    };
    let key = unique_key();
    let err = storage.get(&key).await.unwrap_err();
    assert!(
        matches!(err, BlobStorageError::NotFound(_)),
        "expected NotFound, got {err:?}",
    );
}

#[tokio::test]
async fn s3_head_returns_correct_size() {
    let Some(storage) = setup_storage() else {
        skip("head_size");
        return;
    };
    let key = unique_key();
    storage
        .put(&key, Bytes::from_static(b"12345"))
        .await
        .unwrap();

    let meta = storage.head(&key).await.unwrap();
    assert_eq!(meta.size, 5);
    // SeaweedFS exposes ETag (MD5) — etag should be Some.
    assert!(meta.etag.is_some(), "S3 backend should expose etag");
}

#[tokio::test]
async fn s3_head_missing_returns_not_found() {
    let Some(storage) = setup_storage() else {
        skip("head_missing");
        return;
    };
    let key = unique_key();
    let err = storage.head(&key).await.unwrap_err();
    assert!(matches!(err, BlobStorageError::NotFound(_)));
}

#[tokio::test]
async fn s3_delete_removes_blob() {
    let Some(storage) = setup_storage() else {
        skip("delete_removes");
        return;
    };
    let key = unique_key();
    storage.put(&key, Bytes::from_static(b"x")).await.unwrap();
    storage.delete(&key).await.unwrap();

    let err = storage.get(&key).await.unwrap_err();
    assert!(matches!(err, BlobStorageError::NotFound(_)));
}

#[tokio::test]
async fn s3_put_if_absent_writes_when_absent() {
    let Some(storage) = setup_storage() else {
        skip("put_if_absent_writes");
        return;
    };
    let key = unique_key();
    let outcome = storage
        .put_if_absent(&key, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    assert_eq!(outcome, PutOutcome::Wrote);

    let got = storage.get(&key).await.unwrap();
    assert_eq!(got.as_ref(), b"hello");
}

#[tokio::test]
async fn s3_put_if_absent_skips_when_present() {
    let Some(storage) = setup_storage() else {
        skip("put_if_absent_skips");
        return;
    };
    let key = unique_key();
    storage
        .put_if_absent(&key, Bytes::from_static(b"first"))
        .await
        .unwrap();

    let outcome = storage
        .put_if_absent(&key, Bytes::from_static(b"second"))
        .await
        .unwrap();
    assert_eq!(outcome, PutOutcome::AlreadyExists);

    // Decision #22 invariant: losing PUT must not overwrite the body.
    let got = storage.get(&key).await.unwrap();
    assert_eq!(
        got.as_ref(),
        b"first",
        "losing put_if_absent must leave the original blob untouched",
    );
}

// ----- Phase 2.2 (presigned URLs + server-side multipart) ----------------

/// Phase 2.2: presigned PUT lets the browser upload directly to the S3
/// backend, bypassing the api server (no body relay, no axum body-limit
/// pressure). Verifies the signed URL is accepted by SeaweedFS and the
/// resulting blob is readable via the trait.
#[tokio::test]
async fn s3_presign_put_uploads_via_signed_url() {
    let Some(storage) = setup_storage() else {
        skip("presign_put");
        return;
    };
    let key = unique_key();
    let payload = Bytes::from_static(b"presigned-put-payload");

    let url = storage
        .presign_put(&key, Duration::from_secs(60))
        .await
        .expect("presign_put");

    let response = reqwest::Client::new()
        .put(url.as_str())
        .body(payload.clone())
        .send()
        .await
        .expect("PUT request")
        .error_for_status()
        .expect("PUT 2xx");
    drop(response);

    let got = storage.get(&key).await.unwrap();
    assert_eq!(got, payload);
}

/// Phase 2.2: presigned GET lets the browser fetch large assets directly
/// from the backend (CDN-cacheable, no server bandwidth). Verifies the
/// signed URL returns the previously stored bytes verbatim.
#[tokio::test]
async fn s3_presign_get_downloads_via_signed_url() {
    let Some(storage) = setup_storage() else {
        skip("presign_get");
        return;
    };
    let key = unique_key();
    let payload = Bytes::from_static(b"presigned-get-payload");
    storage.put(&key, payload.clone()).await.unwrap();

    let url = storage
        .presign_get(&key, Duration::from_secs(60))
        .await
        .expect("presign_get");

    let bytes = reqwest::Client::new()
        .get(url.as_str())
        .send()
        .await
        .expect("GET request")
        .error_for_status()
        .expect("GET 2xx")
        .bytes()
        .await
        .expect("body bytes");

    assert_eq!(bytes.as_ref(), payload.as_ref());
}

/// Phase 2.2: server-side multipart upload for the >5 GB single-PUT bypass
/// path (project owner large-data use case). Each chunk is filled with a
/// distinct byte value so reassembly order can be asserted byte-by-byte.
///
/// S3 protocol rule: every part except the last must be at least 5 MiB.
/// We use 5 MiB × 3 = 15 MiB total, which both honours the rule and keeps
/// the test fast on local SeaweedFS (~0.5 s).
#[tokio::test]
async fn s3_multipart_put_chunks_assembles_15mib_blob() {
    let Some(storage) = setup_storage() else {
        skip("multipart_put_chunks");
        return;
    };
    let key = unique_key();

    const CHUNK_SIZE: usize = 5 * 1024 * 1024;
    let chunks = vec![
        Bytes::from(vec![0u8; CHUNK_SIZE]),
        Bytes::from(vec![1u8; CHUNK_SIZE]),
        Bytes::from(vec![2u8; CHUNK_SIZE]),
    ];

    storage
        .multipart_put_chunks(&key, chunks)
        .await
        .expect("multipart_put_chunks");

    let meta = storage.head(&key).await.unwrap();
    assert_eq!(meta.size, (3 * CHUNK_SIZE) as u64);

    let got = storage.get(&key).await.unwrap();
    assert_eq!(got.len(), 3 * CHUNK_SIZE);
    assert!(
        got[0..CHUNK_SIZE].iter().all(|&b| b == 0),
        "chunk 0 must be all 0u8 (assembly order)",
    );
    assert!(
        got[CHUNK_SIZE..2 * CHUNK_SIZE].iter().all(|&b| b == 1),
        "chunk 1 must be all 1u8",
    );
    assert!(
        got[2 * CHUNK_SIZE..3 * CHUNK_SIZE].iter().all(|&b| b == 2),
        "chunk 2 must be all 2u8",
    );
}
