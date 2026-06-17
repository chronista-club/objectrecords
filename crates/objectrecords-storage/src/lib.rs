//! Object Records — storage layer.
//!
//! This crate is intentionally **independent of `objectrecords-core`**: the
//! core crate encodes the type-state invariants of records, this crate handles
//! the physical I/O of binary blobs. The two are stitched together by the
//! upstream `objectrecords-api` crate (Phase 4) — `core::Record::fix(hash)`
//! consumes a digest computed by the storage layer over the put bytes
//! (Phase 1.5 fossilization metaphor, decision #14: `core` stays
//! dependency-free).
//!
//! # Backend strategy
//!
//! - Phase 2.0 (this commit): `InMemoryStorage` for tests and small dev loops.
//! - Phase 2.1: `S3CompatStorage` against MinIO / garage / さくらクラウド
//!   オブジェクトストレージ (decision #9).
//!
//! Single-backend MVP per decision #12; the
//! [`BlobStorage`] trait is generic-friendly so callers do not need
//! `Arc<dyn BlobStorage>` until a multi-backend requirement actually arises.
//!
//! # Storage key conventions
//!
//! - `Record<Mutable>` / `Record<Snapshot>` blobs: callers pick any key
//!   (typically a UUID v7 namespace). Migratory-bird metaphor (decision #15):
//!   the key is fluid until fossilization.
//! - `Record<Fixed>` blobs: `core` rewrites the trailing version's
//!   `BlobRef.key` to `/fixed/<sha256>` at `fix()` time, and the storage
//!   layer is expected to host that path (and dedup by content hash —
//!   decision #11, internal design notes).
//!
//! This crate does not enforce the key shape; that is `core`'s job.

use std::future::Future;

use bytes::Bytes;

// =============================================================================
// Metadata
// =============================================================================

/// Metadata returned by [`BlobStorage::head`].
///
/// Phase 2.0 keeps this minimal — only the size, which is the field every
/// caller needs (CreoID-authenticated upload validation, `BlobRef.size`
/// reconciliation, etc). Additional fields (etag, content-type, last-modified)
/// will be added when concrete callers need them (additive-only, decision
/// principle #3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMetadata {
    /// Payload size in bytes.
    pub size: u64,
    /// Backend-supplied etag (e.g., S3 ETag header). `None` for in-memory
    /// backend.
    pub etag: Option<String>,
}

// =============================================================================
// Error
// =============================================================================

/// Errors returned by [`BlobStorage`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum BlobStorageError {
    /// The requested key does not exist in the backend.
    #[error("blob not found: {0}")]
    NotFound(String),
    /// Backend-specific failure (network, I/O, permission, etc).
    ///
    /// Wrapping a boxed error keeps this enum stable across backend swaps —
    /// a future S3 backend can surface its native error type without forcing
    /// a breaking change here.
    #[error("storage backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

// =============================================================================
// Conditional-write outcome
// =============================================================================

/// Result of a conditional write via [`BlobStorage::put_if_absent`].
///
/// Maps to S3's `If-None-Match: *` semantics (decision #22): a 200 response
/// is [`Self::Wrote`] (we won the race and the bytes were stored), a 412
/// "Precondition Failed" response is [`Self::AlreadyExists`] (someone else
/// already committed the same key, so our PUT was a no-op).
///
/// `bool` would carry the same information, but the named variants make
/// callers' branches self-documenting at the dedup-commit boundary, which is
/// load-bearing for `Record<Fixed>` content addressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The key was absent and we wrote the blob.
    Wrote,
    /// The key already existed; the existing blob was preserved and our
    /// payload was discarded (server-side, atomically).
    AlreadyExists,
}

// =============================================================================
// Trait
// =============================================================================

/// Object Records' minimal blob CRUD contract.
///
/// Methods are desugared to `fn ... -> impl Future<Output = ...> + Send`
/// rather than `async fn` in the trait declaration. Reason: `async fn` in a
/// public trait does not let us spell the auto-trait bounds, and Phase 4
/// (axum) requires the returned futures to be `Send` so handlers can be
/// spawned on the multi-thread tokio runtime. Implementors can still write
/// `async fn` in their `impl` blocks — the compiler desugars them and
/// preserves `Send` provided the body itself is `Send`.
///
/// `dyn BlobStorage` is intentionally not supported in Phase 2.0
/// (single-backend MVP, decision #12); use
/// generic `<S: BlobStorage>` parameters at the call site. `dyn` support is
/// additive (e.g., via `trait_variant::make`) when a multi-backend
/// requirement actually arises.
///
/// The `Send + Sync` super-traits make implementors compatible with axum
/// state and tokio's multi-thread runtime when Phase 4 lands.
pub trait BlobStorage: Send + Sync {
    /// Stores `blob` under `key`. Overwrites any existing value at `key`
    /// (S3 semantics).
    fn put(
        &self,
        key: &str,
        blob: Bytes,
    ) -> impl Future<Output = Result<(), BlobStorageError>> + Send;

    /// Reads the blob at `key`. Returns [`BlobStorageError::NotFound`] if no
    /// blob exists at `key`.
    fn get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Bytes, BlobStorageError>> + Send;

    /// Returns metadata for the blob at `key` without transferring the body.
    /// Returns [`BlobStorageError::NotFound`] if no blob exists at `key`.
    fn head(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<BlobMetadata, BlobStorageError>> + Send;

    /// Removes the blob at `key`. Returns [`BlobStorageError::NotFound`] if
    /// no blob exists at `key` (callers that want idempotent delete should
    /// match the error).
    fn delete(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<(), BlobStorageError>> + Send;

    /// Conditional write: stores `blob` only if no value currently exists at
    /// `key`. Maps to S3's `If-None-Match: *` (decision #22 — `mem` pending
    /// memory id, see CLAUDE.md). Returns [`PutOutcome::Wrote`] if we wrote
    /// the blob, or [`PutOutcome::AlreadyExists`] if the key was already
    /// occupied (server-side atomic — no race window).
    ///
    /// Phase 4 (api) calls this exactly at the `Record::fix` commit boundary,
    /// so concurrent `fix()` calls that hash to the same `/fixed/<sha256>`
    /// converge on a single physical blob (decision #11 dedup) without the
    /// caller running its own check-then-set logic.
    fn put_if_absent(
        &self,
        key: &str,
        blob: Bytes,
    ) -> impl Future<Output = Result<PutOutcome, BlobStorageError>> + Send;
}

// =============================================================================
// In-memory backend
// =============================================================================

pub mod s3_compat;

/// In-memory backend, intended for tests and small dev loops.
pub mod in_memory {
    use std::collections::HashMap;
    use std::collections::hash_map::Entry;
    use std::sync::Mutex;

    use bytes::Bytes;

    use super::{BlobMetadata, BlobStorage, BlobStorageError, PutOutcome};

    /// `HashMap`-backed [`BlobStorage`] suitable for tests.
    ///
    /// Uses `std::sync::Mutex` rather than `tokio::sync::Mutex` because no
    /// `await` point is held across the lock — the in-memory operations
    /// complete synchronously inside each `async fn`, so the lock is released
    /// before the future yields. Keeps the crate runtime-agnostic.
    #[derive(Default, Debug)]
    pub struct InMemoryStorage {
        inner: Mutex<HashMap<String, Bytes>>,
    }

    impl InMemoryStorage {
        /// Creates an empty in-memory storage.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl BlobStorage for InMemoryStorage {
        async fn put(&self, key: &str, blob: Bytes) -> Result<(), BlobStorageError> {
            self.inner
                .lock()
                .expect("in-memory storage mutex poisoned")
                .insert(key.to_owned(), blob);
            Ok(())
        }

        async fn get(&self, key: &str) -> Result<Bytes, BlobStorageError> {
            self.inner
                .lock()
                .expect("in-memory storage mutex poisoned")
                .get(key)
                .cloned()
                .ok_or_else(|| BlobStorageError::NotFound(key.to_owned()))
        }

        async fn head(&self, key: &str) -> Result<BlobMetadata, BlobStorageError> {
            let guard = self
                .inner
                .lock()
                .expect("in-memory storage mutex poisoned");
            let blob = guard
                .get(key)
                .ok_or_else(|| BlobStorageError::NotFound(key.to_owned()))?;
            Ok(BlobMetadata {
                size: blob.len() as u64,
                etag: None,
            })
        }

        async fn delete(&self, key: &str) -> Result<(), BlobStorageError> {
            let removed = self
                .inner
                .lock()
                .expect("in-memory storage mutex poisoned")
                .remove(key);
            match removed {
                Some(_) => Ok(()),
                None => Err(BlobStorageError::NotFound(key.to_owned())),
            }
        }

        async fn put_if_absent(
            &self,
            key: &str,
            blob: Bytes,
        ) -> Result<PutOutcome, BlobStorageError> {
            let mut guard = self
                .inner
                .lock()
                .expect("in-memory storage mutex poisoned");
            match guard.entry(key.to_owned()) {
                Entry::Occupied(_) => Ok(PutOutcome::AlreadyExists),
                Entry::Vacant(slot) => {
                    slot.insert(blob);
                    Ok(PutOutcome::Wrote)
                }
            }
        }
    }
}

// =============================================================================
// Tests (Small tier, Phase 2.0)
// =============================================================================

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::in_memory::InMemoryStorage;
    use super::{BlobStorage, BlobStorageError, PutOutcome};

    #[tokio::test]
    async fn put_then_get_returns_same_blob() {
        let storage = InMemoryStorage::new();
        let payload = Bytes::from_static(b"hello world");
        storage.put("k1", payload.clone()).await.unwrap();

        let got = storage.get("k1").await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn get_missing_returns_not_found() {
        let storage = InMemoryStorage::new();
        let err = storage.get("absent").await.unwrap_err();
        assert!(matches!(err, BlobStorageError::NotFound(k) if k == "absent"));
    }

    #[tokio::test]
    async fn head_returns_correct_size() {
        let storage = InMemoryStorage::new();
        storage
            .put("size-check", Bytes::from_static(b"12345"))
            .await
            .unwrap();

        let meta = storage.head("size-check").await.unwrap();
        assert_eq!(meta.size, 5);
        assert!(meta.etag.is_none(), "in-memory backend has no etag");
    }

    #[tokio::test]
    async fn head_missing_returns_not_found() {
        let storage = InMemoryStorage::new();
        let err = storage.head("absent").await.unwrap_err();
        assert!(matches!(err, BlobStorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_blob() {
        let storage = InMemoryStorage::new();
        storage
            .put("doomed", Bytes::from_static(b"x"))
            .await
            .unwrap();
        storage.delete("doomed").await.unwrap();

        let err = storage.get("doomed").await.unwrap_err();
        assert!(matches!(err, BlobStorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_missing_returns_not_found() {
        let storage = InMemoryStorage::new();
        let err = storage.delete("absent").await.unwrap_err();
        assert!(matches!(err, BlobStorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn put_overwrites_existing_blob() {
        let storage = InMemoryStorage::new();
        storage
            .put("k", Bytes::from_static(b"old"))
            .await
            .unwrap();
        storage
            .put("k", Bytes::from_static(b"new"))
            .await
            .unwrap();

        let got = storage.get("k").await.unwrap();
        assert_eq!(got.as_ref(), b"new");
    }

    // ----- Phase 2.0.1 (decision #22 conditional writes) ----------------------

    #[tokio::test]
    async fn put_if_absent_writes_when_absent() {
        let storage = InMemoryStorage::new();
        let outcome = storage
            .put_if_absent("/fixed/abc", Bytes::from_static(b"hello"))
            .await
            .unwrap();
        assert_eq!(outcome, PutOutcome::Wrote);

        let got = storage.get("/fixed/abc").await.unwrap();
        assert_eq!(got.as_ref(), b"hello");
    }

    #[tokio::test]
    async fn put_if_absent_skips_when_present() {
        let storage = InMemoryStorage::new();
        storage
            .put_if_absent("/fixed/abc", Bytes::from_static(b"first"))
            .await
            .unwrap();

        let outcome = storage
            .put_if_absent("/fixed/abc", Bytes::from_static(b"second"))
            .await
            .unwrap();
        assert_eq!(outcome, PutOutcome::AlreadyExists);
    }

    #[tokio::test]
    async fn put_if_absent_preserves_original_blob_on_collision() {
        // Decision #22 invariant: a losing PUT must NOT overwrite the byte
        // payload, the ETag, or any side-effect attached to the existing key
        // (Object Lock retain-until is the load-bearing example).
        let storage = InMemoryStorage::new();
        storage
            .put_if_absent("/fixed/abc", Bytes::from_static(b"first"))
            .await
            .unwrap();
        let _ignored = storage
            .put_if_absent("/fixed/abc", Bytes::from_static(b"second"))
            .await
            .unwrap();

        let got = storage.get("/fixed/abc").await.unwrap();
        assert_eq!(
            got.as_ref(),
            b"first",
            "losing PUT must leave the original blob untouched",
        );
    }
}
