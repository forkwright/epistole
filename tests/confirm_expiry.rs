//! Expiry handling on the `POST /confirm` surface (the commit path;
//! `GET /confirm` never writes — see tests/consent_contract.rs).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
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
async fn expired_confirm_token_is_refused_and_creates_no_subscriber() {
    use secrecy::ExposeSecret;

    // Issue #44: no test covered the expired-token path through the full
    // HTTP handlers. `verify` rejects on expiry, and `POST /confirm` is
    // the handler that turns a valid token into a durable Active row —
    // so the load-bearing assertion is that no row is written, not just
    // that the invalid-link page renders.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg));

    // Mint a Confirm token that expired an hour ago. Everything else
    // about it is valid: correct kind, correct signature, live secret,
    // generation 0 (matches the "no row yet" baseline).
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = epistole::token::Token::new(
        epistole::token::TokenKind::Confirm,
        "expired@example.com".to_owned(),
        now - 3600,
        0,
    );
    let signed =
        epistole::token::sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/confirm")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", "203.0.113.90")
                .body(Body::from(format!("token={signed}")))
                .expect("req"),
        )
        .await
        .expect("response");

    // Rejections are deliberately shaped as 200 + invalid-link page.
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");
    assert!(
        html.contains("This link has expired."),
        "expired token must render the invalid-link page, got: {html}"
    );

    // The durable half: an expired token must not create the Active row.
    let sub = store.subscriber_get("expired@example.com").expect("read");
    assert!(
        sub.is_none(),
        "an expired confirm token must not create a subscriber row (got {sub:?})"
    );
}
