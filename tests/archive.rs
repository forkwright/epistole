//! Integration tests for the public archive endpoints.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use epistole::{
    Config, Store,
    config::{Brand, Smtp},
    router,
};
use http_body_util::BodyExt;
use secrecy::SecretString;
use tempfile::TempDir;
use tower::ServiceExt;

fn test_config(data_dir: std::path::PathBuf) -> Config {
    Config {
        bind: "127.0.0.1:0".to_owned(),
        data_dir,
        base_url: "https://letters.example.com".to_owned(),
        brand: Brand {
            name: "Test Brand".to_owned(),
            from_address: "letters@example.com".to_owned(),
            reply_to: None,
        },
        smtp: Smtp {
            host: "127.0.0.1".to_owned(),
            port: 0,
            username: "user".to_owned(),
            password: SecretString::from("pass".to_owned()),
        },
        token_secret: SecretString::from("test-secret-32-bytes-padding-aaaa".to_owned()),
        send_auth_token: SecretString::from("operator-bearer-test".to_owned()),
    }
}

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
