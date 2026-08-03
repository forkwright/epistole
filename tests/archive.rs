//! Integration tests for the public archive endpoints.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use epistole::{Store, router};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

mod common;
use common::test_config;

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn archive_lists_sends_and_links_to_detail_pages() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let mut send_ids = Vec::new();
    for subject in ["First note", "Second note"] {
        let payload = format!("{{\"subject\":\"{subject}\",\"markdown\":\"# {subject}\"}}");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/send")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer operator-bearer-test")
                    .header("x-forwarded-for", "203.0.113.70")
                    .body(Body::from(payload))
                    .expect("req"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        send_ids.push(json["send_id"].as_str().expect("send_id").to_owned());
    }

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/archive")
                .header("x-forwarded-for", "203.0.113.71")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let cache = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .expect("cache-control")
        .to_str()
        .expect("cache-control utf8");
    assert_eq!(cache, "public, max-age=300");
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");
    assert!(html.contains("First note"));
    assert!(html.contains("Second note"));
    for send_id in send_ids {
        assert!(html.contains(&format!("/archive/{send_id}")));
    }
    // An index below the cap is the complete history, so it must not
    // claim to be truncated. Pairs with
    // `archive_index_caps_the_page_and_reports_the_truncation`.
    assert!(
        !html.contains("Showing the most recent"),
        "an untruncated index must not report truncation"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn archive_detail_renders_send_body_with_immutable_cache() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("authorization", "Bearer operator-bearer-test")
                .header("x-forwarded-for", "203.0.113.72")
                .body(Body::from(
                    "{\"subject\":\"Archive detail\",\"markdown\":\"## Body\\n\\n[link](https://example.com)\"}",
                ))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let send_id = json["send_id"].as_str().expect("send_id");

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/archive/{send_id}"))
                .header("x-forwarded-for", "203.0.113.73")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let cache = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .expect("cache-control")
        .to_str()
        .expect("cache-control utf8");
    assert_eq!(cache, "public, max-age=31536000, immutable");
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");
    assert!(html.contains("Archive detail"));
    assert!(html.contains("<h2>Body</h2>"));
    assert!(html.contains(r#"href="https://example.com""#));
    assert!(html.contains(">link</a>"));
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn archive_detail_returns_404_for_missing_send() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/archive/01J00000000000000000000000")
                .header("x-forwarded-for", "203.0.113.74")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn archive_index_caps_the_page_and_reports_the_truncation() {
    // Issue #44: GET /archive materialized every send on every request.
    // The index now reads at most a fixed number of the newest sends, so
    // this asserts both halves: the oldest sends fall off the page, and
    // the page says it is showing only the most recent ones.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    // Two past the 100-record cap, so the two oldest must be excluded.
    // Send ids are ULIDs, so insertion order is also archive order.
    for i in 0..102 {
        let payload = format!("{{\"subject\":\"Note {i:03}\",\"markdown\":\"# body\"}}");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/send")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer operator-bearer-test")
                    .body(Body::from(payload))
                    .expect("req"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/archive")
                .header("x-forwarded-for", "203.0.113.75")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");

    // The two oldest are past the cap and must not be rendered. Without
    // the cap every one of the 102 appears, so these two assertions are
    // what fail if the bound is removed.
    assert!(
        !html.contains("Note 000"),
        "oldest send past the cap must not be rendered"
    );
    assert!(
        !html.contains("Note 001"),
        "second-oldest send past the cap must not be rendered"
    );

    // The newest, and the oldest that still fits, are both present.
    assert!(html.contains("Note 101"), "newest send must be rendered");
    assert!(
        html.contains("Note 002"),
        "last send inside the cap must be rendered"
    );

    // Exactly the cap is rendered.
    assert_eq!(
        html.matches("<li>").count(),
        100,
        "archive index must render exactly the capped number of sends"
    );

    assert!(
        html.contains("Showing the most recent 100 notes."),
        "a truncated index must say it is truncated, got: {html}"
    );
}
