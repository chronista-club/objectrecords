//! Phase 4.F — storage backend env wiring.
//!
//! Resolves [`objectrecords_storage::s3_compat::S3CompatConfig`] from
//! `OBJECTRECORDS_S3_*` env vars, with the same all-or-nothing
//! discipline as [`crate::auth::auth_env_from_vars`] (Phase 4.E):
//!
//! - **All required vars set** → [`StorageEnv::Enabled`] with full config.
//! - **All required vars absent** → [`StorageEnv::Disabled`] (legacy
//!   scratch deploys without `POST /assets` support; write endpoints
//!   that require storage will surface 500 with a clear log line).
//! - **Partial set** → [`StorageEnvError`] (fail-loud — never silently
//!   degrade to InMemory or absent).
//!
//! Required vars: `_ENDPOINT`, `_BUCKET`, `_ACCESS_KEY`, `_SECRET_KEY`.
//! Optional: `_REGION` (default `us-east-1` — SeaweedFS / RustFS accept
//! anything, AWS / R2 need actual region), `_ALLOW_HTTP` (default
//! `false`; set `true` for local SeaweedFS over plain HTTP).
//!
//! Output is purposely a config-only enum; the actual
//! [`S3CompatStorage`][objectrecords_storage::s3_compat::S3CompatStorage]
//! is built by the caller via `S3CompatStorage::from_config(_)` so
//! tests can assert on shape without instantiating the backend.

use thiserror::Error;

/// Env var holding the S3 endpoint URL (e.g.
/// `http://localhost:8333` for the storage host
/// SeaweedFS Quadlet).
pub const ENV_S3_ENDPOINT: &str = "OBJECTRECORDS_S3_ENDPOINT";
/// Env var holding the bucket name (e.g. `objectrecords-live`).
/// Object Records does not auto-create buckets; provision out-of-band.
pub const ENV_S3_BUCKET: &str = "OBJECTRECORDS_S3_BUCKET";
/// Env var holding the region label. Optional, default `"us-east-1"`.
pub const ENV_S3_REGION: &str = "OBJECTRECORDS_S3_REGION";
/// Env var holding the access key id.
pub const ENV_S3_ACCESS_KEY: &str = "OBJECTRECORDS_S3_ACCESS_KEY";
/// Env var holding the secret access key.
pub const ENV_S3_SECRET_KEY: &str = "OBJECTRECORDS_S3_SECRET_KEY";
/// Env var enabling plain-HTTP (non-TLS) backends. Optional, default
/// `false`. Set `"true"` (case-insensitive) for local SeaweedFS at
/// `http://...`.
pub const ENV_S3_ALLOW_HTTP: &str = "OBJECTRECORDS_S3_ALLOW_HTTP";

/// Resolved storage env, ready to be turned into an
/// [`objectrecords_storage::s3_compat::S3CompatStorage`] by the
/// caller via `S3CompatStorage::from_config(_)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageEnv {
    /// No storage configured. Write endpoints that require storage
    /// will fail with `500 Internal` + a log line. Read endpoints
    /// and inline-only writes still function.
    Disabled,
    /// Full S3-compat config ready for `S3CompatStorage::from_config`.
    Enabled {
        /// Endpoint URL.
        endpoint: String,
        /// Bucket name.
        bucket: String,
        /// Region label.
        region: String,
        /// Access key id.
        access_key: String,
        /// Secret access key.
        secret_key: String,
        /// Whether plain HTTP is allowed.
        allow_http: bool,
    },
}

/// A partial S3 configuration: some — but not all — of the required
/// `OBJECTRECORDS_S3_*` env vars were set. Fail-loud.
#[derive(Debug, Error)]
#[error(
    "incomplete S3 config — set all of [{ENV_S3_ENDPOINT}, \
     {ENV_S3_BUCKET}, {ENV_S3_ACCESS_KEY}, {ENV_S3_SECRET_KEY}] \
     to enable storage, or none to disable; unset: {missing}"
)]
pub struct StorageEnvError {
    /// Comma-separated names of the env vars left unset.
    pub missing: String,
}

/// Resolves [`StorageEnv`] from a variable-lookup closure.
///
/// `get` is injected (not [`std::env`] direct) so the all-or-nothing
/// rule is unit-testable without mutating process-global env. `get`
/// should return `None` for both unset *and* empty/whitespace values
/// — callers normalise (`env::var(...).ok().filter(...)`).
///
/// # Errors
/// Returns [`StorageEnvError`] when the 4 required S3 vars are
/// partially set. `region` / `allow_http` are optional with defaults
/// and never contribute to the error condition.
pub fn storage_env_from_vars(
    get: impl Fn(&str) -> Option<String>,
) -> Result<StorageEnv, StorageEnvError> {
    let endpoint = get(ENV_S3_ENDPOINT);
    let bucket = get(ENV_S3_BUCKET);
    let access_key = get(ENV_S3_ACCESS_KEY);
    let secret_key = get(ENV_S3_SECRET_KEY);

    match (&endpoint, &bucket, &access_key, &secret_key) {
        (None, None, None, None) => Ok(StorageEnv::Disabled),
        (Some(endpoint), Some(bucket), Some(access_key), Some(secret_key)) => {
            let region = get(ENV_S3_REGION).unwrap_or_else(|| "us-east-1".to_string());
            let allow_http = get(ENV_S3_ALLOW_HTTP)
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false);
            Ok(StorageEnv::Enabled {
                endpoint: endpoint.clone(),
                bucket: bucket.clone(),
                region,
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                allow_http,
            })
        }
        _ => {
            let missing = [
                (ENV_S3_ENDPOINT, &endpoint),
                (ENV_S3_BUCKET, &bucket),
                (ENV_S3_ACCESS_KEY, &access_key),
                (ENV_S3_SECRET_KEY, &secret_key),
            ]
            .into_iter()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<Vec<_>>()
            .join(", ");
            Err(StorageEnvError { missing })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn all_unset_resolves_to_disabled() {
        let env = storage_env_from_vars(lookup(&[])).expect("no vars valid");
        assert_eq!(env, StorageEnv::Disabled);
    }

    #[test]
    fn all_required_set_resolves_to_enabled_with_defaults() {
        let env = storage_env_from_vars(lookup(&[
            (ENV_S3_ENDPOINT, "http://localhost:8333"),
            (ENV_S3_BUCKET, "objectrecords-live"),
            (ENV_S3_ACCESS_KEY, "ak"),
            (ENV_S3_SECRET_KEY, "sk"),
        ]))
        .expect("4 required vars is valid");
        assert_eq!(
            env,
            StorageEnv::Enabled {
                endpoint: "http://localhost:8333".to_string(),
                bucket: "objectrecords-live".to_string(),
                region: "us-east-1".to_string(),
                access_key: "ak".to_string(),
                secret_key: "sk".to_string(),
                allow_http: false,
            }
        );
    }

    #[test]
    fn region_override_applied() {
        let env = storage_env_from_vars(lookup(&[
            (ENV_S3_ENDPOINT, "https://r2.example.com"),
            (ENV_S3_BUCKET, "or"),
            (ENV_S3_ACCESS_KEY, "ak"),
            (ENV_S3_SECRET_KEY, "sk"),
            (ENV_S3_REGION, "auto"),
        ]))
        .unwrap();
        match env {
            StorageEnv::Enabled { region, .. } => assert_eq!(region, "auto"),
            other => panic!("expected Enabled, got {other:?}"),
        }
    }

    #[test]
    fn allow_http_truthy_values() {
        for v in ["true", "True", "TRUE", "1", "yes", "on"] {
            let env = storage_env_from_vars(lookup(&[
                (ENV_S3_ENDPOINT, "http://localhost:8333"),
                (ENV_S3_BUCKET, "b"),
                (ENV_S3_ACCESS_KEY, "ak"),
                (ENV_S3_SECRET_KEY, "sk"),
                (ENV_S3_ALLOW_HTTP, v),
            ]))
            .unwrap();
            match env {
                StorageEnv::Enabled { allow_http, .. } => assert!(allow_http, "expected truthy for {v:?}"),
                other => panic!("expected Enabled, got {other:?}"),
            }
        }
    }

    #[test]
    fn allow_http_falsy_default() {
        let env = storage_env_from_vars(lookup(&[
            (ENV_S3_ENDPOINT, "https://r2.example.com"),
            (ENV_S3_BUCKET, "b"),
            (ENV_S3_ACCESS_KEY, "ak"),
            (ENV_S3_SECRET_KEY, "sk"),
        ]))
        .unwrap();
        match env {
            StorageEnv::Enabled { allow_http, .. } => assert!(!allow_http),
            other => panic!("expected Enabled, got {other:?}"),
        }
    }

    #[test]
    fn partial_config_is_rejected_loudly() {
        let err = storage_env_from_vars(lookup(&[
            (ENV_S3_ENDPOINT, "http://localhost:8333"),
            (ENV_S3_ACCESS_KEY, "ak"),
        ]))
        .expect_err("missing bucket + secret_key");
        assert!(!err.missing.contains(ENV_S3_ENDPOINT));
        assert!(!err.missing.contains(ENV_S3_ACCESS_KEY));
        assert!(err.missing.contains(ENV_S3_BUCKET));
        assert!(err.missing.contains(ENV_S3_SECRET_KEY));
    }

    #[test]
    fn partial_with_optional_doesnt_trip_error() {
        // Region set + others unset → still all-absent path, region
        // alone doesn't drag config into Enabled territory.
        let env = storage_env_from_vars(lookup(&[(ENV_S3_REGION, "ap-northeast-1")])).expect(
            "only optional set should yield Disabled because no required vars are set",
        );
        assert_eq!(env, StorageEnv::Disabled);
    }
}
