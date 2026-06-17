//! S3-Compatible [`BlobStorage`] backend, built on `object_store`.
//!
//! This module wraps [`object_store::aws::AmazonS3`] so that any S3-compatible
//! backend can serve as the physical store for Object Records. The Phase 2.0
//! decision chain pins three concrete targets:
//!
//! - **MVP local** (decision #23, 2026-04-29): SeaweedFS 4.22+ via the dev
//!   stack already running for `creo-memories`. `If-None-Match: *` is
//!   verified by experiment + by source review of upstream PR #7154 / #8802.
//! - **Production** (decision #9, partial supersede pending #24): さくら
//!   クラウド オブジェクトストレージ. Conditional-write support is unverified
//!   at the time of writing — check before promoting.
//! - **Future MVP local re-eval** (decision #23): RustFS 1.0 GA + e2e test
//!   pass-rate growth, 6-12 months out from 2026-04.
//!
//! The crate stays runtime-agnostic (no `tokio` import here) and keeps the
//! native `async fn in trait` desugar (decision #19) so the returned futures
//! are `Send`-compatible with axum / multi-thread tokio.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::signer::Signer;
use object_store::{Error as ObjectStoreError, ObjectStore, ObjectStoreExt, PutMode, PutOptions};

use crate::{BlobMetadata, BlobStorage, BlobStorageError, PutOutcome};

/// Re-export so callers can spell URLs without depending on `url` directly.
pub use url::Url;

/// Configuration for [`S3CompatStorage`].
///
/// Designed for **single-backend MVP** (decision #12) — the same builder
/// drives SeaweedFS, RustFS, AWS S3, R2, and さくら simply by swapping
/// `endpoint` / `region` / credentials. No multi-backend abstraction is
/// layered on top; `BlobStorage` is the only seam.
#[derive(Debug, Clone)]
pub struct S3CompatConfig {
    /// Endpoint URL (e.g., `http://localhost:12100` for local SeaweedFS,
    /// `https://s3.amazonaws.com` for AWS S3, vendor-specific for さくら).
    pub endpoint: String,
    /// Region label. SeaweedFS / RustFS accept any value (`us-east-1` is a
    /// safe default); AWS S3 / R2 require the bucket's actual region.
    pub region: String,
    /// Bucket name. Object Records does not manage buckets — pre-create with
    /// `aws s3 mb s3://<bucket>` (or vendor equivalent).
    pub bucket: String,
    /// Access key id.
    pub access_key: String,
    /// Secret access key.
    pub secret_key: String,
    /// Allow plain HTTP. Set `true` for local self-host backends, `false`
    /// (default) for production.
    pub allow_http: bool,
}

/// [`BlobStorage`] implementation backed by [`object_store::aws::AmazonS3`].
///
/// Internally holds an `Arc<AmazonS3>` so the storage can be shared across
/// async tasks without cloning the underlying HTTP client / connection pool.
#[derive(Debug, Clone)]
pub struct S3CompatStorage {
    inner: Arc<AmazonS3>,
}

impl S3CompatStorage {
    /// Builds the underlying `AmazonS3` client from `config`.
    ///
    /// # Errors
    ///
    /// Returns [`BlobStorageError::Backend`] if `object_store` rejects the
    /// configuration (malformed endpoint URL, etc).
    pub fn from_config(config: S3CompatConfig) -> Result<Self, BlobStorageError> {
        let inner = AmazonS3Builder::new()
            .with_endpoint(config.endpoint)
            .with_region(config.region)
            .with_bucket_name(config.bucket)
            .with_access_key_id(config.access_key)
            .with_secret_access_key(config.secret_key)
            .with_allow_http(config.allow_http)
            .build()
            .map_err(|e| BlobStorageError::Backend(Box::new(e)))?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Generates a presigned URL for a `PUT` request to `key`, valid for
    /// `ttl`.
    ///
    /// Phase 4 (api crate) hands this URL to a browser so the upload bypasses
    /// the api server entirely (no body relay, no axum body-limit pressure,
    /// no server bandwidth). The browser PUTs directly to the S3-compatible
    /// backend within `ttl` (typically 15 minutes).
    ///
    /// Single-PUT size limit applies: 5 GB on AWS S3 / SeaweedFS / R2. For
    /// larger uploads use [`Self::multipart_put_chunks`] (server-side) or
    /// the future client-side multipart presigned flow (deferred).
    ///
    /// # Errors
    ///
    /// Returns [`BlobStorageError::Backend`] if signing fails.
    pub async fn presign_put(
        &self,
        key: &str,
        ttl: Duration,
    ) -> Result<Url, BlobStorageError> {
        self.inner
            .signed_url(http::Method::PUT, &Path::from(key), ttl)
            .await
            .map_err(map_err)
    }

    /// Generates a presigned URL for a `GET` request to `key`, valid for
    /// `ttl`.
    ///
    /// Phase 4 returns this URL in API responses for asset download so the
    /// browser fetches blobs directly from the storage backend (no server
    /// bandwidth, CDN-cacheable).
    ///
    /// # Errors
    ///
    /// Returns [`BlobStorageError::Backend`] if signing fails.
    pub async fn presign_get(
        &self,
        key: &str,
        ttl: Duration,
    ) -> Result<Url, BlobStorageError> {
        self.inner
            .signed_url(http::Method::GET, &Path::from(key), ttl)
            .await
            .map_err(map_err)
    }

    /// Server-side multipart upload: stores a blob assembled from `chunks`
    /// at `key`.
    ///
    /// Used by Phase 4 (api crate) when an upload exceeds the 5 GB single-PUT
    /// limit (project owner's large-data use case). The function enforces
    /// no minimum chunk size — callers are responsible for honouring the S3
    /// rule that **all parts except the last must be at least 5 MiB**
    /// (ten thousand parts maximum on AWS S3; SeaweedFS / R2 have similar
    /// caps).
    ///
    /// On any per-part failure the [`object_store::MultipartUpload`] is
    /// dropped without `complete()`, which triggers the implicit `abort()`
    /// path documented by `object_store` — no orphan parts on the backend.
    ///
    /// # Errors
    ///
    /// Returns [`BlobStorageError::Backend`] if any of `put_multipart`,
    /// `put_part`, or `complete` fails. The path is propagated where
    /// possible; otherwise it is opaque.
    pub async fn multipart_put_chunks(
        &self,
        key: &str,
        chunks: Vec<Bytes>,
    ) -> Result<(), BlobStorageError> {
        let mut upload = self
            .inner
            .put_multipart(&Path::from(key))
            .await
            .map_err(map_err)?;
        for chunk in chunks {
            upload.put_part(chunk.into()).await.map_err(map_err)?;
        }
        upload.complete().await.map_err(map_err)?;
        Ok(())
    }
}

/// Maps `object_store` errors onto [`BlobStorageError`].
///
/// Only `NotFound` is special-cased; everything else is wrapped as
/// `Backend(_)` to keep the error enum stable across backends — callers
/// should not dispatch on backend-specific error variants.
fn map_err(err: ObjectStoreError) -> BlobStorageError {
    match err {
        ObjectStoreError::NotFound { path, .. } => BlobStorageError::NotFound(path),
        other => BlobStorageError::Backend(Box::new(other)),
    }
}

impl BlobStorage for S3CompatStorage {
    async fn put(&self, key: &str, blob: Bytes) -> Result<(), BlobStorageError> {
        self.inner
            .put(&Path::from(key), blob.into())
            .await
            .map(|_| ())
            .map_err(map_err)
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobStorageError> {
        let result = self.inner.get(&Path::from(key)).await.map_err(map_err)?;
        result.bytes().await.map_err(map_err)
    }

    async fn head(&self, key: &str) -> Result<BlobMetadata, BlobStorageError> {
        let meta = self.inner.head(&Path::from(key)).await.map_err(map_err)?;
        Ok(BlobMetadata {
            size: meta.size,
            etag: meta.e_tag,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), BlobStorageError> {
        self.inner
            .delete(&Path::from(key))
            .await
            .map_err(map_err)
    }

    async fn put_if_absent(
        &self,
        key: &str,
        blob: Bytes,
    ) -> Result<PutOutcome, BlobStorageError> {
        // PutMode::Create translates to S3's `If-None-Match: *` header
        // (decision #22). On collision, object_store returns
        // `ObjectStoreError::AlreadyExists` which we map to
        // `PutOutcome::AlreadyExists` — the losing PUT must NOT mutate the
        // existing blob (Phase 2.0.1 invariant tested in
        // `InMemoryStorage::put_if_absent_preserves_original_blob_on_collision`).
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match self
            .inner
            .put_opts(&Path::from(key), blob.into(), opts)
            .await
        {
            Ok(_) => Ok(PutOutcome::Wrote),
            Err(ObjectStoreError::AlreadyExists { .. }) => Ok(PutOutcome::AlreadyExists),
            Err(other) => Err(map_err(other)),
        }
    }
}
