//! Phase 4.0 in-process smoke tests for the api crate.
//!
//! Uses `tower::ServiceExt::oneshot` so the test never binds a TCP
//! socket — the request flows through axum's stack in-memory, which
//! keeps the test in the millisecond range while still exercising
//! routing, extractors, the AppState wiring, and the JSON
//! serialisation path. Same env-gated idiom as the db crate's
//! `surrealdb_local.rs`: gated by `OBJECTRECORDS_SURREAL_TEST_ENDPOINT`.
//!
//! Phase 4.1+ will introduce JWT-aware tests; for now Phase 4.0
//! exercises the no-auth happy path.

use std::env;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http::header::CONTENT_TYPE;
use objectrecords_api::{AppState, build_router};
use objectrecords_core::{Body as CoreBody, Kind, Mutable, Record};
use objectrecords_db::{
    ObjectRecordsDb, ObjectRecordsDbConfig, RecordRepository, SurrealRecordRepository,
};
use tower::ServiceExt;
use uuid::Uuid;

async fn setup_app() -> Option<(Router, SurrealRecordRepository)> {
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
    let app = build_router(AppState::new(repo.clone()));
    Some((app, repo))
}

fn skip(test: &str) {
    eprintln!("[skip] {test}: OBJECTRECORDS_SURREAL_TEST_ENDPOINT not set");
}

fn unique_creator() -> String {
    format!("creo_user:test-{}", Uuid::new_v4())
}

#[tokio::test]
async fn get_health_returns_ok_status() {
    let Some((app, _repo)) = setup_app().await else {
        skip("get_health_returns_ok_status");
        return;
    };
    let response = app
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .expect("oneshot health");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "expected JSON content-type, got {content_type:?}",
    );
}

#[tokio::test]
async fn get_record_returns_200_and_attribution_for_existing_id() {
    let Some((app, repo)) = setup_app().await else {
        skip("get_record_returns_200_and_attribution_for_existing_id");
        return;
    };
    let creator = unique_creator();
    let record =
        Record::<Mutable>::new(Kind::Log, CoreBody::Inline(b"phase-4.0".to_vec()));
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
        .expect("oneshot get_record");
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("response body");
    let body_str = std::str::from_utf8(&body_bytes).expect("utf-8");
    // Sanity-check: response carries the creator (Paradigm Ⅱ
    // attribution, decision #33) and the id we asked for.
    assert!(
        body_str.contains(&creator),
        "response must carry the creator literal, got: {body_str}",
    );
    assert!(
        body_str.contains(&id.to_string()),
        "response must carry the record id, got: {body_str}",
    );

    repo.delete(id).await.unwrap();
}

#[tokio::test]
async fn get_record_returns_404_for_unknown_id() {
    let Some((app, _repo)) = setup_app().await else {
        skip("get_record_returns_404_for_unknown_id");
        return;
    };
    let id = Uuid::now_v7();

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/records/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("oneshot get_record_unknown");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Body must include the kind discriminator for machine-readable
    // dispatch (decision Sub-Q5).
    let body_bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("response body");
    let body_str = std::str::from_utf8(&body_bytes).expect("utf-8");
    assert!(
        body_str.contains("\"kind\":\"NotFound\""),
        "expected error.kind=NotFound, got: {body_str}",
    );
}
