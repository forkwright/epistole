//! Consent-generation binding for the consent-token contract
//! (forkwright/epistole#65): a confirm/unsubscribe token is bound to
//! the subscriber's consent generation at mint time, so a token minted
//! BEFORE a later unsubscribe stays refused while one minted AFTER it
//! is honored — in both directions.
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
async fn fresh_confirm_token_minted_after_unsubscribe_reactivates_the_subscriber() {
    // The core forkwright/epistole#65 fix: a re-subscribe issued AFTER
    // an unsubscribe must be able to bring the address back, which the
    // old blanket "state == Unsubscribed => always refuse" logic could
    // never allow.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());
    let secret = cfg.token_secret.expose_secret().as_bytes();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let xff = "203.0.113.105";

    // 1. Subscribe + confirm at generation 0 -> Active.
    let confirm0 = sign(
        &Token::new(
            TokenKind::Confirm,
            "returning@example.com".to_owned(),
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

    // 2. Unsubscribe at generation 0 -> Unsubscribed, generation bumps to 1.
    let unsub0 = sign(
        &Token::new(
            TokenKind::Unsubscribe,
            "returning@example.com".to_owned(),
            now + 3600,
            0,
        ),
        secret,
    )
    .expect("sign");
    let _ = app
        .clone()
        .oneshot(post_form("/unsubscribe", xff, format!("token={unsub0}")))
        .await
        .expect("response");
    let after_unsub = store
        .subscriber_get("returning@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(after_unsub.state, SubscriberState::Unsubscribed);
    assert_eq!(after_unsub.generation, 1);

    // 3. A FRESH confirm token minted at the post-unsubscribe generation
    //    (what mint_confirm_token would produce for a re-subscribe now).
    let confirm1 = sign(
        &Token::new(
            TokenKind::Confirm,
            "returning@example.com".to_owned(),
            now + 3600,
            after_unsub.generation,
        ),
        secret,
    )
    .expect("sign");
    let resp = app
        .oneshot(post_form("/confirm", xff, format!("token={confirm1}")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");
    assert!(
        html.contains("Subscribed."),
        "a confirm token minted AFTER the unsubscribe must be able to reactivate: {html}"
    );

    let final_state = store
        .subscriber_get("returning@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(final_state.state, SubscriberState::Active);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
#[expect(
    clippy::too_many_lines,
    reason = "end-to-end scenario test — splitting hides the linear walk"
)]
async fn stale_unsubscribe_token_cannot_cancel_a_later_opt_in() {
    // The mirror of the classic bug: an unsubscribe token captured
    // during an EARLIER Active period must not be able to cancel a
    // LATER, independent opt-in once the consent generation has moved
    // on past it.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());
    let secret = cfg.token_secret.expose_secret().as_bytes();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let xff = "203.0.113.106";

    // 1. Subscribe + confirm -> Active (generation 0).
    let confirm0 = sign(
        &Token::new(
            TokenKind::Confirm,
            "cycled@example.com".to_owned(),
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

    // 2. Capture (but don't discard) an unsubscribe token at generation
    //    0, then USE it: Active -> Unsubscribed, generation bumps to 1.
    let stale_unsub = sign(
        &Token::new(
            TokenKind::Unsubscribe,
            "cycled@example.com".to_owned(),
            now + 3600,
            0,
        ),
        secret,
    )
    .expect("sign");
    let _ = app
        .clone()
        .oneshot(post_form(
            "/unsubscribe",
            xff,
            format!("token={stale_unsub}"),
        ))
        .await
        .expect("response");

    // 3. A later, independent opt-in cycle: fresh confirm at generation
    //    1 -> Active again.
    let confirm1 = sign(
        &Token::new(
            TokenKind::Confirm,
            "cycled@example.com".to_owned(),
            now + 3600,
            1,
        ),
        secret,
    )
    .expect("sign");
    let _ = app
        .clone()
        .oneshot(post_form("/confirm", xff, format!("token={confirm1}")))
        .await
        .expect("response");
    let before_replay = store
        .subscriber_get("cycled@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(before_replay.state, SubscriberState::Active);

    // 4. Replay the OLD (generation-0) unsubscribe token. It already
    //    succeeded once at step 2; replaying it now must NOT cancel the
    //    later opt-in from step 3.
    let resp = app
        .oneshot(post_form(
            "/unsubscribe",
            xff,
            format!("token={stale_unsub}"),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let html = std::str::from_utf8(&body).expect("utf8");
    assert!(
        html.contains("This link has expired."),
        "a stale unsubscribe token replayed after a later opt-in must land on the \
         invalid-link page, got: {html}"
    );

    let after_replay = store
        .subscriber_get("cycled@example.com")
        .expect("read")
        .expect("subscriber");
    assert_eq!(
        after_replay.state,
        SubscriberState::Active,
        "an old unsubscribe token must not cancel a later opt-in"
    );
}
