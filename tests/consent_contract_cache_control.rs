//! `Cache-Control: no-store` for the consent-token contract
//! (forkwright/epistole#68): every response on the `/confirm` and
//! `/unsubscribe` surfaces carries it, since each reflects live,
//! per-visitor subscriber state.
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
use axum::http::{Request, header};
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

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn post_confirm_and_unsubscribe_responses_carry_no_store() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());
    let xff = "203.0.113.104";

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let confirm_tok = Token::new(
        TokenKind::Confirm,
        "cache-check@example.com".to_owned(),
        now + 3600,
        0,
    );
    let confirm_signed =
        sign(&confirm_tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    let resp = app
        .clone()
        .oneshot(post_form(
            "/confirm",
            xff,
            format!("token={confirm_signed}"),
        ))
        .await
        .expect("response");
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "POST /confirm's outcome page reflects live state and must not be cached"
    );

    let generation = store
        .subscriber_get("cache-check@example.com")
        .expect("read")
        .expect("subscriber")
        .generation;
    let unsub_tok = Token::new(
        TokenKind::Unsubscribe,
        "cache-check@example.com".to_owned(),
        now + 3600,
        generation,
    );
    let unsub_signed = sign(&unsub_tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");
    let resp = app
        .oneshot(post_form(
            "/unsubscribe",
            xff,
            format!("token={unsub_signed}"),
        ))
        .await
        .expect("response");
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
}
