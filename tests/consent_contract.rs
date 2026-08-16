//! The unified consent-token contract from forkwright/epistole#68 + #65:
//!
//! - `GET`/`HEAD` on `/confirm` and `/unsubscribe` never write to the
//!   store, no matter how valid the token (RFC 9110 §9.2.1, #68).
//! - Every response on those two surfaces carries `Cache-Control:
//!   no-store` (#68).
//! - A confirm/unsubscribe token is bound to the subscriber's consent
//!   generation at mint time, so a token minted BEFORE a later
//!   unsubscribe stays refused while one minted AFTER it is honored —
//!   in both directions (#65).
//! - `POST /unsubscribe/one-click` is the RFC 8058
//!   `List-Unsubscribe-Post` contract, distinct from the manual
//!   interstitial-driven `POST /unsubscribe` (#68).
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
use common::{TRUSTED_PROXY_IP, test_config};

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
    let app = router(Arc::clone(&store), Arc::clone(&cfg));

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
    let app = router(Arc::clone(&store), Arc::clone(&cfg));

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
    let app = router(Arc::clone(&store), Arc::clone(&cfg));
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

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn post_confirm_and_unsubscribe_responses_carry_no_store() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg));
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
    let app = router(Arc::clone(&store), Arc::clone(&cfg));
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
    let app = router(Arc::clone(&store), Arc::clone(&cfg));
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

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn one_click_unsubscribe_with_the_rfc_8058_body_unsubscribes() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg));
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
    let app = router(Arc::clone(&store), Arc::clone(&cfg));
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
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let resp = app
        .oneshot(get(
            "/unsubscribe/one-click?token=whatever",
            "203.0.113.109",
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
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
    let app = router(Arc::clone(&store), Arc::clone(&cfg));
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
    let app = router(Arc::clone(&store), Arc::clone(&cfg));
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
