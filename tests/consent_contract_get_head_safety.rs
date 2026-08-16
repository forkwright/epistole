//! GET/HEAD non-mutation for the consent-token contract
//! (forkwright/epistole#68): `GET`/`HEAD` on `/confirm` and
//! `/unsubscribe` never write to the store, no matter how valid the
//! token (RFC 9110 §9.2.1).
//!
//! Every request below sets `x-forwarded-for` AND a trusted-proxy
//! `ConnectInfo` (via `trusted_peer()`): `/confirm`, `/unsubscribe`, and
//! `/unsubscribe/one-click` all sit inside `public_routes`, which
//! carries the per-IP `GovernorLayer`. `TrustedProxyExtractor` only
//! honors `X-Forwarded-For` from a peer listed in `trusted_proxies`
//! (forkwright/epistole#67) — the test harness calls the router
//! directly via `.oneshot()` rather than through
//! `into_make_service_with_connect_info`, so without an injected
//! `ConnectInfo` there is no verified peer at all and the request 500s
//! before reaching the handler; see `src/main.rs`'s
//! `into_make_service_with_connect_info` comment.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use epistole::store::SubscriberState;
use epistole::token::{Token, TokenKind, sign};
use epistole::{Store, router};
use http_body_util::BodyExt;
use secrecy::ExposeSecret;
use tempfile::TempDir;
use tower::ServiceExt;

mod common;
use common::{TRUSTED_PROXY_IP, test_config, test_mailer};

/// The `ConnectInfo` extension axum's `into_make_service_with_connect_info`
/// injects in production — see `tests/integration.rs`'s copy of this
/// helper for why every `tests/*.rs` file needs its own.
fn trusted_peer() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::new(TRUSTED_PROXY_IP, 0))
}

#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn post_form(uri: &str, xff: &str, form_body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-forwarded-for", xff)
        .extension(trusted_peer())
        .body(Body::from(form_body))
        .expect("req")
}

#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn get(uri: &str, xff: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-forwarded-for", xff)
        .extension(trusted_peer())
        .body(Body::empty())
        .expect("req")
}

#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
fn head(uri: &str, xff: &str) -> Request<Body> {
    Request::builder()
        .method("HEAD")
        .uri(uri)
        .header("x-forwarded-for", xff)
        .extension(trusted_peer())
        .body(Body::empty())
        .expect("req")
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn get_confirm_with_a_valid_token_does_not_mutate() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = Token::new(
        TokenKind::Confirm,
        "prefetched@example.com".to_owned(),
        now + 3600,
        0,
    );
    let signed = sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    // A mail-client link-prefetcher or scanner does exactly this: GET
    // the URL, nothing more.
    let resp = app
        .oneshot(get(&format!("/confirm?token={signed}"), "203.0.113.101"))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");
    assert!(
        html.contains("Confirm your subscription?"),
        "a fresh valid token previews the interstitial, not the confirmed page: {html}"
    );

    assert!(
        store
            .subscriber_get("prefetched@example.com")
            .expect("read")
            .is_none(),
        "GET /confirm with a valid, unexpired, correctly-kinded token must not create a row"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn head_confirm_with_a_valid_token_does_not_mutate() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = Token::new(
        TokenKind::Confirm,
        "head-prefetched@example.com".to_owned(),
        now + 3600,
        0,
    );
    let signed = sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    let resp = app
        .oneshot(head(&format!("/confirm?token={signed}"), "203.0.113.102"))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(
        store
            .subscriber_get("head-prefetched@example.com")
            .expect("read")
            .is_none(),
        "HEAD /confirm must not create a row — axum serves it from the same GET handler"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn get_and_head_unsubscribe_with_a_valid_token_do_not_mutate() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());
    let xff = "203.0.113.103";

    // Get a real Active subscriber on the books first, so there is
    // something a mutation-shaped request COULD change.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let confirm_tok = Token::new(
        TokenKind::Confirm,
        "active-reader@example.com".to_owned(),
        now + 3600,
        0,
    );
    let confirm_signed =
        sign(&confirm_tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");
    let _ = app
        .clone()
        .oneshot(post_form(
            "/confirm",
            xff,
            format!("token={confirm_signed}"),
        ))
        .await
        .expect("response");
    let before = store
        .subscriber_get("active-reader@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(before.state, SubscriberState::Active);

    let unsub_tok = Token::new(
        TokenKind::Unsubscribe,
        "active-reader@example.com".to_owned(),
        now + 3600,
        before.generation,
    );
    let unsub_signed = sign(&unsub_tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    let get_resp = app
        .clone()
        .oneshot(get(&format!("/unsubscribe?token={unsub_signed}"), xff))
        .await
        .expect("response");
    assert_eq!(get_resp.status(), StatusCode::OK);
    assert_eq!(
        get_resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let body = get_resp
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");
    assert!(
        html.contains("Unsubscribe?"),
        "a valid unsubscribe token previews the interstitial: {html}"
    );

    let head_resp = app
        .clone()
        .oneshot(head(&format!("/unsubscribe?token={unsub_signed}"), xff))
        .await
        .expect("response");
    assert_eq!(head_resp.status(), StatusCode::OK);

    let after = store
        .subscriber_get("active-reader@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(
        after.state,
        SubscriberState::Active,
        "GET and HEAD /unsubscribe with a valid token must not flip an Active subscriber"
    );
    assert_eq!(
        after.generation, before.generation,
        "a read-only preview must not advance the consent generation either"
    );
}
