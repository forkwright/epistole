//! Integration tests for the epistole HTTP surface. Builds the real
//! router against a tempdir-backed fjall keyspace and exercises the
//! end-to-end happy path.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
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
async fn healthz_returns_ok() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn subscribe_then_confirm_round_trip() {
    use secrecy::ExposeSecret;

    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg));

    // POST /subscribe
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/subscribe")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", "203.0.113.7")
                .body(Body::from("email=alice%40example.com"))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    // Subscriber should be in Pending state.
    let sub = store
        .subscriber_get("alice@example.com")
        .expect("read")
        .expect("subscriber present");
    assert!(matches!(
        sub.state,
        epistole::store::SubscriberState::Pending
    ));

    // Mint a confirm token directly (production path mints inside the
    // handler and mails it; we replay the same code).
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = epistole::token::Token::new(
        epistole::token::TokenKind::Confirm,
        "alice@example.com".to_owned(),
        now + 3600,
    );
    let signed =
        epistole::token::sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    // GET /confirm?token=...
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/confirm?token={signed}"))
                .header("x-forwarded-for", "203.0.113.7")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    // Subscriber now Active.
    let sub = store
        .subscriber_get("alice@example.com")
        .expect("read")
        .expect("subscriber present");
    assert!(matches!(
        sub.state,
        epistole::store::SubscriberState::Active
    ));
    assert!(sub.confirmed_at.is_some());
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn subscribe_rate_limit_kicks_in_under_burst() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    // Burst budget is 6; 7th from same IP should be 429.
    let mut last_status = StatusCode::OK;
    for i in 0..8u32 {
        let body = format!("email=user{i}%40example.com");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/subscribe")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("x-forwarded-for", "198.51.100.42")
                    .body(Body::from(body))
                    .expect("req"),
            )
            .await
            .expect("response");
        last_status = resp.status();
    }
    assert_eq!(last_status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn subscribe_body_limit_rejects_oversized_post() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    // SUBSCRIBE_BODY_LIMIT is 4 KiB; send 8 KiB.
    let oversized = "a".repeat(8 * 1024);
    let body = format!("email={oversized}%40example.com");
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/subscribe")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", "198.51.100.43")
                .body(Body::from(body))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_with_correct_bearer_succeeds() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("authorization", "Bearer operator-bearer-test")
                .header("x-forwarded-for", "203.0.113.99")
                .body(Body::from(
                    "{\"subject\":\"Hello\",\"markdown\":\"# Hi\\n\\nbody\"}",
                ))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_with_wrong_bearer_returns_401() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("authorization", "Bearer wrong-token-of-correct-length")
                .header("x-forwarded-for", "203.0.113.100")
                .body(Body::from("{\"subject\":\"Hello\",\"markdown\":\"# Hi\"}"))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_requires_bearer() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("x-forwarded-for", "203.0.113.7")
                .body(Body::from("{\"subject\":\"Hi\",\"markdown\":\"# Hello\"}"))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
