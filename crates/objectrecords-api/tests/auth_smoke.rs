//! Phase 4.D Step 3 — Auth0 JWT verify middleware POC tests.
//!
//! Three cases per the handoff internal design notes:
//! 1. **valid token** → 200 OK on `GET /records/{id}`
//! 2. **invalid token** (signed by a foreign key) → 401 Unauthorized
//! 3. **missing Authorization header** → 401 Unauthorized
//!
//! The verifier is constructed from a JWKS synthesised at test
//! startup: an in-test RSA-2048 keypair is generated via the `rsa`
//! crate, its public key is encoded into a single-entry JwkSet, and a
//! matching `EncodingKey` is used to sign the valid token. The "invalid
//! token" case uses a **second**, foreign keypair so the signature
//! check fails the way it would on a real attacker-forged token.
//!
//! Same env-gated idiom as `http_smoke.rs`: tests skip silently if the
//! live SurrealDB endpoint env var is absent.

#![allow(clippy::unwrap_used)]

use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::header::AUTHORIZATION;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use objectrecords_api::{AppState, Auth0Verifier, build_router};
use objectrecords_core::{Body as CoreBody, Kind, Mutable, Record};
use objectrecords_db::{
    ObjectRecordsDb, ObjectRecordsDbConfig, RecordRepository, SurrealRecordRepository,
};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::Serialize;
use tower::ServiceExt;
use uuid::Uuid;

const TEST_AUDIENCE: &str = "https://test.objectrecords.io/api";
const TEST_ISSUER: &str = "https://test.local/";
const TEST_KID: &str = "test-key-1";

#[derive(Debug, Serialize)]
struct TestClaims {
    sub: String,
    aud: String,
    iss: String,
    exp: i64,
    iat: i64,
}

/// Owns a private key plus the matching single-entry JWKS JSON. Used to
/// (a) sign tokens for the "valid" test case and (b) configure the
/// [`Auth0Verifier`] under test.
struct TestKey {
    encoding: EncodingKey,
    jwks_json: String,
}

impl TestKey {
    fn generate(kid: &str) -> Self {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let public = RsaPublicKey::from(&private);

        let pem = private.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();
        let encoding = EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();

        let n_b64 = URL_SAFE_NO_PAD.encode(public.n().to_bytes_be());
        let e_b64 = URL_SAFE_NO_PAD.encode(public.e().to_bytes_be());

        // Hand-roll the JWKS payload — keeps the test independent of
        // any specific jsonwebtoken-internal struct shape.
        let jwks_json = format!(
            r#"{{"keys":[{{"kty":"RSA","alg":"RS256","use":"sig","kid":"{kid}","n":"{n_b64}","e":"{e_b64}"}}]}}"#
        );

        Self {
            encoding,
            jwks_json,
        }
    }

    fn sign(&self, claims: &TestClaims, kid: &str) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        encode(&header, claims, &self.encoding).unwrap()
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn valid_claims(sub: &str) -> TestClaims {
    let now = now_secs();
    TestClaims {
        sub: sub.to_string(),
        aud: TEST_AUDIENCE.to_string(),
        iss: TEST_ISSUER.to_string(),
        exp: now + i64::try_from(Duration::from_secs(3600).as_secs()).unwrap(),
        iat: now,
    }
}

async fn setup_app() -> Option<(Router, SurrealRecordRepository, TestKey)> {
    let endpoint = env::var("OBJECTRECORDS_SURREAL_TEST_ENDPOINT").ok()?;
    let namespace = env::var("OBJECTRECORDS_SURREAL_TEST_NAMESPACE")
        .unwrap_or_else(|_| "objectrecords_test".to_string());
    let database = env::var("OBJECTRECORDS_SURREAL_TEST_DATABASE")
        .unwrap_or_else(|_| "main".to_string());
    let username = env::var("OBJECTRECORDS_SURREAL_TEST_USERNAME")
        .unwrap_or_else(|_| "admin".to_string());
    let password = env::var("OBJECTRECORDS_SURREAL_TEST_PASSWORD")
        .unwrap_or_else(|_| "admin-local-dev".to_string());

    let db = ObjectRecordsDb::connect(ObjectRecordsDbConfig {
        endpoint,
        namespace,
        database,
        username,
        password,
    })
    .await
    .expect("connect to surreal-local");
    let repo = SurrealRecordRepository::new(db);

    let test_key = TestKey::generate(TEST_KID);
    let verifier =
        Auth0Verifier::from_jwks_json(&test_key.jwks_json, TEST_AUDIENCE, TEST_ISSUER).unwrap();
    let app = build_router(AppState::with_auth(repo.clone(), verifier));
    Some((app, repo, test_key))
}

fn skip(test: &str) {
    eprintln!("[skip] {test}: OBJECTRECORDS_SURREAL_TEST_ENDPOINT not set");
}

fn unique_creator() -> String {
    format!("creo_user:test-{}", Uuid::new_v4())
}

#[tokio::test]
async fn valid_token_returns_200_on_records_endpoint() {
    let Some((app, repo, test_key)) = setup_app().await else {
        skip("valid_token_returns_200_on_records_endpoint");
        return;
    };
    let creator = unique_creator();
    let record = Record::<Mutable>::new(Kind::Log, CoreBody::Inline(b"phase-4.D-step-3".to_vec()));
    let id = record.id();
    repo.save(&record, &creator).await.unwrap();

    let token = test_key.sign(&valid_claims("auth0|test-user-1"), TEST_KID);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/records/{id}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot valid");
    assert_eq!(response.status(), StatusCode::OK);

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn invalid_token_returns_401() {
    let Some((app, repo, _test_key)) = setup_app().await else {
        skip("invalid_token_returns_401");
        return;
    };
    let creator = unique_creator();
    let record = Record::<Mutable>::new(Kind::Log, CoreBody::Inline(b"phase-4.D-step-3".to_vec()));
    let id = record.id();
    repo.save(&record, &creator).await.unwrap();

    // Sign with a *different* key — same kid, foreign signature.
    let foreign = TestKey::generate(TEST_KID);
    let token = foreign.sign(&valid_claims("auth0|attacker"), TEST_KID);

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/records/{id}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot invalid");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body_bytes).unwrap();
    assert!(
        body_str.contains("\"kind\":\"Unauthorized\""),
        "expected error.kind=Unauthorized, got: {body_str}",
    );

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn missing_token_returns_401() {
    let Some((app, repo, _test_key)) = setup_app().await else {
        skip("missing_token_returns_401");
        return;
    };
    let creator = unique_creator();
    let record = Record::<Mutable>::new(Kind::Log, CoreBody::Inline(b"phase-4.D-step-3".to_vec()));
    let id = record.id();
    repo.save(&record, &creator).await.unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/records/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot missing");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn health_endpoint_unprotected_even_with_auth_enabled() {
    let Some((app, _repo, _test_key)) = setup_app().await else {
        skip("health_endpoint_unprotected_even_with_auth_enabled");
        return;
    };
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot health");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "GET /health must remain open for probes regardless of auth state",
    );
}

/// Phase 4.E Step 5 — `aud` claim mismatch must fail closed (401).
/// This is the 4th case of the canonical verify test matrix
/// (valid / invalid sig / missing header / wrong aud).
#[tokio::test]
async fn wrong_audience_returns_401() {
    let Some((app, repo, test_key)) = setup_app().await else {
        skip("wrong_audience_returns_401");
        return;
    };
    let creator = unique_creator();
    let record = Record::<Mutable>::new(Kind::Log, CoreBody::Inline(b"phase-4.E-step-5".to_vec()));
    let id = record.id();
    repo.save(&record, &creator).await.unwrap();

    let now = now_secs();
    let bad_aud_claims = TestClaims {
        sub: "auth0|test-user-wrong-aud".to_string(),
        aud: "https://wrong.example.com/api".to_string(),
        iss: TEST_ISSUER.to_string(),
        exp: now + 3600,
        iat: now,
    };
    let token = test_key.sign(&bad_aud_claims, TEST_KID);

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/records/{id}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot wrong_aud");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    repo.delete(id).await.unwrap();
}

/// Phase 4.E Step 5b — JIT provisioning. The first valid token bearing
/// a previously-unseen sub triggers an `or_user` insert; the second call
/// for the same sub returns the same row (idempotent).
#[tokio::test]
async fn first_auth_provisions_or_user_idempotently() {
    let Some((app, repo, test_key)) = setup_app().await else {
        skip("first_auth_provisions_or_user_idempotently");
        return;
    };
    // Use a non-`creo_user:`-prefixed sub so this test doesn't collide
    // with the existing-record test's creator literal idiom.
    let test_sub = format!("auth0|test-{}", Uuid::new_v4());

    // First call — protected endpoint, valid token. The middleware
    // will upsert or_user, then proceed. We don't care about the
    // record itself (404 is fine), only the side effect.
    let token = test_key.sign(&valid_claims(&test_sub), TEST_KID);
    let bogus_id = Uuid::now_v7();
    let resp1 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/records/{bogus_id}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot first");
    // Either 200 (if the record happened to exist) or 404 (won't);
    // both prove the middleware passed and or_user was inserted.
    assert_ne!(resp1.status(), StatusCode::UNAUTHORIZED);
    assert_ne!(resp1.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // Verify the or_user row landed.
    let first = repo
        .upsert_or_user(&test_sub)
        .await
        .expect("upsert reads existing");
    assert_eq!(first.auth0_sub, test_sub);

    // Second auth call — same sub. The upsert should hit the SELECT
    // fast-path and return the same id.
    let resp2 = app
        .oneshot(
            Request::builder()
                .uri(format!("/records/{bogus_id}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot second");
    assert_ne!(resp2.status(), StatusCode::UNAUTHORIZED);

    let second = repo
        .upsert_or_user(&test_sub)
        .await
        .expect("upsert reads existing again");
    assert_eq!(
        first.id, second.id,
        "JIT provisioning must be idempotent — same sub → same or_user id",
    );
    assert_eq!(first.created_at, second.created_at);
}

/// Phase 4.E Step 6 / 4 投資 #2 — `X-MCP-Client-Id` is **optional** and
/// must not gate auth (only audited). With a valid token, the request
/// succeeds regardless of whether the header is present.
#[tokio::test]
async fn mcp_client_id_header_accepted_does_not_gate_auth() {
    let Some((app, repo, test_key)) = setup_app().await else {
        skip("mcp_client_id_header_accepted_does_not_gate_auth");
        return;
    };
    let creator = unique_creator();
    let record = Record::<Mutable>::new(Kind::Log, CoreBody::Inline(b"phase-4.E-step-6".to_vec()));
    let id = record.id();
    repo.save(&record, &creator).await.unwrap();

    let token = test_key.sign(&valid_claims("auth0|test-user-mcp"), TEST_KID);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/records/{id}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header("x-mcp-client-id", "creo-mcp-server-test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot mcp_header");
    assert_eq!(response.status(), StatusCode::OK);

    repo.delete(id).await.unwrap();
}
