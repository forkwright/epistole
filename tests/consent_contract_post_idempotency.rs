//! POST idempotency for the consent-token contract: replaying the
//! same `POST /confirm` or `POST /unsubscribe` token must not write
//! to the store, or bump the consent generation, a second time.
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
use axum::http::{Request, StatusCode};
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

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn post_confirm_applied_twice_does_not_double_write() {
    // Idempotency for the state-modifying POST /confirm: re-submitting
    // the same token (double form-submit, browser retry) must not touch
    // the store a second time. Checking only the externally-visible
    // state ("still Active") wouldn't distinguish a no-op second call
    // from one that re-wrote the identical values, so the load-bearing
    // assertion is confirmed_at staying byte-identical across both
    // calls — a second `subscriber_put` would stamp a later `now`.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());
    let xff = "203.0.113.110";

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let signed = sign(
        &Token::new(
            TokenKind::Confirm,
            "double-submit@example.com".to_owned(),
            now + 3600,
            0,
        ),
        cfg.token_secret.expose_secret().as_bytes(),
    )
    .expect("sign");

    let first = app
        .clone()
        .oneshot(post_form("/confirm", xff, format!("token={signed}")))
        .await
        .expect("response");
    assert_eq!(first.status(), StatusCode::OK);
    let after_first = store
        .subscriber_get("double-submit@example.com")
        .expect("read")
        .expect("subscriber created by the first POST");
    assert_eq!(after_first.state, SubscriberState::Active);

    let second = app
        .oneshot(post_form("/confirm", xff, format!("token={signed}")))
        .await
        .expect("response");
    assert_eq!(second.status(), StatusCode::OK);
    let after_second = store
        .subscriber_get("double-submit@example.com")
        .expect("read")
        .expect("subscriber");

    assert_eq!(after_second.state, SubscriberState::Active);
    assert_eq!(
        after_second.confirmed_at, after_first.confirmed_at,
        "a second POST /confirm with the same token must not write again \
         (confirmed_at would advance if it did)"
    );
    assert_eq!(
        after_second.generation, after_first.generation,
        "the idempotent replay must not advance the consent generation either"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn post_unsubscribe_applied_twice_bumps_generation_exactly_once() {
    // Idempotency for the state-modifying POST /unsubscribe, mirroring
    // the send_id idempotency shape (same input twice -> one effect):
    // applying the SAME unsubscribe token twice must transition the
    // consent generation exactly once, not once per request. The first
    // call is the real transition (Active -> Unsubscribed, generation
    // 0 -> 1); the second call's token still carries generation 0, but
    // the row is already Unsubscribed, so it must land on the
    // idempotent no-write arm rather than being evaluated against the
    // (now-stale) generation at all.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());
    let secret = cfg.token_secret.expose_secret().as_bytes();
    let xff = "203.0.113.111";
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    let confirm0 = sign(
        &Token::new(
            TokenKind::Confirm,
            "unsub-twice@example.com".to_owned(),
            now + 3600,
            0,
        ),
        secret,
    )
    .expect("sign");
    let _ = app
        .clone()
        .oneshot(post_form("/confirm", xff, format!("token={confirm0}")))
        .await
        .expect("response");

    let unsub0 = sign(
        &Token::new(
            TokenKind::Unsubscribe,
            "unsub-twice@example.com".to_owned(),
            now + 3600,
            0,
        ),
        secret,
    )
    .expect("sign");

    let first = app
        .clone()
        .oneshot(post_form("/unsubscribe", xff, format!("token={unsub0}")))
        .await
        .expect("response");
    assert_eq!(first.status(), StatusCode::OK);
    let after_first = store
        .subscriber_get("unsub-twice@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(after_first.state, SubscriberState::Unsubscribed);
    assert_eq!(after_first.generation, 1);

    let second = app
        .oneshot(post_form("/unsubscribe", xff, format!("token={unsub0}")))
        .await
        .expect("response");
    assert_eq!(second.status(), StatusCode::OK);
    let body = second.into_body().collect().await.expect("body").to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");
    assert!(
        html.contains("Unsubscribed."),
        "a replayed unsubscribe token must still render the (idempotent) success page: {html}"
    );

    let after_second = store
        .subscriber_get("unsub-twice@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(after_second.state, SubscriberState::Unsubscribed);
    assert_eq!(
        after_second.generation, 1,
        "replaying the same unsubscribe token must not bump the generation a second time"
    );
}
