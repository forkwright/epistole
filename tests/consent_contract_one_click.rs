//! RFC 8058 one-click unsubscribe for the consent-token contract
//! (forkwright/epistole#68): `POST /unsubscribe/one-click` is the
//! `List-Unsubscribe-Post` contract, distinct from the manual
//! interstitial-driven `POST /unsubscribe`.
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

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn one_click_unsubscribe_with_the_rfc_8058_body_unsubscribes() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());
    let secret = cfg.token_secret.expose_secret().as_bytes();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let xff = "203.0.113.107";

    let confirm0 = sign(
        &Token::new(
            TokenKind::Confirm,
            "one-click@example.com".to_owned(),
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
            "one-click@example.com".to_owned(),
            now + 3600,
            0,
        ),
        secret,
    )
    .expect("sign");

    // The exact wire shape a mail client sends per RFC 8058 §3.1: POST
    // to the List-Unsubscribe URL (token in the query string) with body
    // `List-Unsubscribe=One-Click`. No interstitial, no prior GET.
    let resp = app
        .oneshot(post_form(
            &format!("/unsubscribe/one-click?token={unsub0}"),
            xff,
            "List-Unsubscribe=One-Click".to_owned(),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let sub = store
        .subscriber_get("one-click@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(sub.state, SubscriberState::Unsubscribed);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn one_click_unsubscribe_rejects_a_body_that_is_not_the_rfc_8058_marker() {
    // Negative case: a POST to the one-click URL whose body is anything
    // other than the exact RFC 8058 marker must be refused rather than
    // silently treated as a one-click unsubscribe. Proves the endpoint
    // actually checks the body instead of treating any POST as
    // sufficient authority.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());
    let secret = cfg.token_secret.expose_secret().as_bytes();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let xff = "203.0.113.108";

    let confirm0 = sign(
        &Token::new(
            TokenKind::Confirm,
            "wrong-body@example.com".to_owned(),
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
            "wrong-body@example.com".to_owned(),
            now + 3600,
            0,
        ),
        secret,
    )
    .expect("sign");

    let resp = app
        .oneshot(post_form(
            &format!("/unsubscribe/one-click?token={unsub0}"),
            xff,
            "List-Unsubscribe=Something-Else".to_owned(),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let sub = store
        .subscriber_get("wrong-body@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(
        sub.state,
        SubscriberState::Active,
        "a malformed one-click body must not unsubscribe the address"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn one_click_unsubscribe_is_post_only() {
    // RFC 9110 safety applies here too: nothing about the one-click
    // endpoint is reachable via GET, so a prefetch of a List-Unsubscribe
    // URL (which mail clients sometimes probe) cannot trigger it either.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

    let resp = app
        .oneshot(get(
            "/unsubscribe/one-click?token=whatever",
            "203.0.113.109",
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
