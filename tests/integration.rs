//! Integration tests for the epistole HTTP surface. Builds the real
//! router against a tempdir-backed fjall keyspace and exercises the
//! end-to-end happy path.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use epistole::{SendId, Store, router};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

mod common;
use common::{test_config, test_mailer};

/// A `POST` request with an `application/x-www-form-urlencoded` body —
/// what the confirm/unsubscribe interstitial's own form submits, and
/// what `/subscribe` has always taken.
///
/// `xff` is required, not optional: `/confirm` and `/unsubscribe` sit
/// inside `public_routes`, which carries the per-IP `GovernorLayer`. The
/// test harness calls the router directly via `.oneshot()` rather than
/// through `into_make_service_with_connect_info`, so with no XFF header
/// and no injected `ConnectInfo`, `TrustedProxyExtractor` cannot derive
/// a rate-limit key and the request 500s before reaching the handler —
/// see `src/main.rs`'s `into_make_service_with_connect_info` comment.
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
        .body(Body::from(form_body))
        .expect("req")
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn healthz_returns_ok() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

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
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());

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

    // Subscribe no longer persists a Pending row before mailbox proof.
    let sub = store.subscriber_get("alice@example.com").expect("read");
    assert!(sub.is_none());

    // Mint a confirm token directly (production path mints inside the
    // handler and mails it; we replay the same code). Generation 0
    // matches mint_confirm_token's own "no row yet" baseline.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = epistole::token::Token::new(
        epistole::token::TokenKind::Confirm,
        "alice@example.com".to_owned(),
        now + 3600,
        0,
    );
    let signed =
        epistole::token::sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    // GET /confirm?token=... previews only — no mutation.
    let resp = app
        .clone()
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
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let body_str = std::str::from_utf8(&body).expect("utf8");
    assert!(
        body_str.contains("Confirm your subscription?"),
        "GET must render the interstitial, not the confirmed page: {body_str}"
    );
    assert!(
        store
            .subscriber_get("alice@example.com")
            .expect("read")
            .is_none(),
        "GET /confirm must not have written a subscriber row"
    );

    // POST /confirm — the interstitial's own form submit — is what
    // actually commits.
    let resp = app
        .oneshot(post_form(
            "/confirm",
            "203.0.113.7",
            format!("token={signed}"),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    // Subscriber now exists as Active; POST /confirm is the first
    // durable write.
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
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

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
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

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
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("authorization", "Bearer operator-bearer-test")
                .header("x-forwarded-for", "203.0.113.99")
                .body(Body::from(format!(
                    "{{\"send_id\":\"{}\",\"subject\":\"Hello\",\"markdown\":\"# Hi\\n\\nbody\"}}",
                    SendId::generate()
                )))
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
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

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
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

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

// === Phase 1.5 audit-finding regression tests ===

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn unsubscribed_subscriber_cannot_be_reactivated_via_stale_confirm_token() {
    // Audit findings #7 / #18: a confirm token whose subscriber has
    // since unsubscribed must NOT bring them back to Active. This test
    // exercises the full state machine: subscribe → confirm → unsubscribe
    // → replay original confirm token → should be invalid-link, NOT
    // re-active. Both tokens are minted at generation 0 (the address's
    // starting generation), which is exactly what makes the confirm
    // token stale once unsubscribe bumps to generation 1.
    use secrecy::ExposeSecret;

    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());

    // 1. POST /subscribe
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/subscribe")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", "203.0.113.50")
                .body(Body::from("email=victim%40example.com"))
                .expect("req"),
        )
        .await
        .expect("response");

    // 2. Mint a confirm token (capturing it for replay later).
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let confirm_tok = epistole::token::Token::new(
        epistole::token::TokenKind::Confirm,
        "victim@example.com".to_owned(),
        now + 3600,
        0,
    );
    let confirm_signed =
        epistole::token::sign(&confirm_tok, cfg.token_secret.expose_secret().as_bytes())
            .expect("sign");

    // 3. Confirm → Active (POST — GET no longer mutates).
    let _ = app
        .clone()
        .oneshot(post_form(
            "/confirm",
            "203.0.113.50",
            format!("token={confirm_signed}"),
        ))
        .await
        .expect("response");

    // 4. Unsubscribe with a fresh unsub token, also generation 0 (the
    //    row hasn't moved off it — confirm doesn't bump generation).
    let unsub_tok = epistole::token::Token::new(
        epistole::token::TokenKind::Unsubscribe,
        "victim@example.com".to_owned(),
        now + 3600,
        0,
    );
    let unsub_signed =
        epistole::token::sign(&unsub_tok, cfg.token_secret.expose_secret().as_bytes())
            .expect("sign");
    let _ = app
        .clone()
        .oneshot(post_form(
            "/unsubscribe",
            "203.0.113.50",
            format!("token={unsub_signed}"),
        ))
        .await
        .expect("response");

    // 5. Replay the ORIGINAL confirm token. Its generation (0) no
    //    longer matches the row's (1, bumped by the unsubscribe above),
    //    so it must land on the invalid-link page and NOT re-Activate.
    let _ = app
        .clone()
        .oneshot(post_form(
            "/confirm",
            "203.0.113.50",
            format!("token={confirm_signed}"),
        ))
        .await
        .expect("response");

    let sub = store
        .subscriber_get("victim@example.com")
        .expect("read")
        .expect("subscriber");
    assert!(
        matches!(sub.state, epistole::store::SubscriberState::Unsubscribed),
        "stale confirm token must not re-Activate an Unsubscribed address (was {:?})",
        sub.state
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn subscribe_rejects_display_name_mailbox_form() {
    // Audit finding #9: `email_address::is_valid` with default options
    // accepts the full RFC 5322 mailbox form, which lets an attacker
    // submit `Pwned <victim@example.com>` and email-bomb the victim.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/subscribe")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", "203.0.113.51")
                .body(Body::from("email=Pwned+%3Cvictim%40example.com%3E"))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn token_round_trip_survives_pipe_in_email() {
    // Audit finding #9 (second half): email containing `|` in local part
    // is RFC-legal but the old token format mis-parsed via `splitn(3, '|')`.
    // After the base64-encoded inner-email fix, this round-trips.
    let secret = b"this-is-only-for-tests-32-bytes!";
    let tok = epistole::token::Token::new(
        epistole::token::TokenKind::Confirm,
        "weird|name@example.com".to_owned(),
        9_999_999_999,
        0,
    );
    let signed = epistole::token::sign(&tok, secret).expect("sign");
    let verified = epistole::token::verify(&signed, secret, 0).expect("verify");
    assert_eq!(verified.email, "weird|name@example.com");
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn rate_limit_keys_on_last_xff_entry_only() {
    // Audit findings #5 / #14: rotating X-Forwarded-For chain does NOT
    // bypass per-IP rate limiting. We use a CONSTANT last entry across
    // 8 requests (simulating NPM setting it to the real client IP) but
    // a varying earlier hop (simulating a hostile client trying to spoof
    // its way to a fresh bucket). The 7th request must 429.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

    let mut last_status = StatusCode::OK;
    for i in 0..8u32 {
        let body = format!("email=spoofer{i}%40example.com");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/subscribe")
                    .header("content-type", "application/x-www-form-urlencoded")
                    // Rotating earlier hop, but the LAST entry (NPM-set)
                    // is constant: 198.51.100.99. The extractor MUST
                    // key on .99, not the rotating prefix.
                    .header("x-forwarded-for", format!("10.0.0.{i}, 198.51.100.99"))
                    .body(Body::from(body))
                    .expect("req"),
            )
            .await
            .expect("response");
        last_status = resp.status();
    }
    assert_eq!(
        last_status,
        StatusCode::TOO_MANY_REQUESTS,
        "rotating XFF prefix must not bypass per-IP rate limiting"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn stateless_confirm_with_no_subscriber_creates_active_row() {
    // Issue #5: confirmation is stateless. A valid signed confirm token
    // proves mailbox ownership, so the subscriber row is created only here,
    // not during /subscribe.
    use secrecy::ExposeSecret;

    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());

    // Mint a valid confirm token for an address that's never been
    // persisted — generation 0, the same baseline mint_confirm_token
    // reads for a row that doesn't exist yet.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = epistole::token::Token::new(
        epistole::token::TokenKind::Confirm,
        "ghost@example.com".to_owned(),
        now + 3600,
        0,
    );
    let signed =
        epistole::token::sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    let resp = app
        .oneshot(post_form(
            "/confirm",
            "203.0.113.52",
            format!("token={signed}"),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let body_str = std::str::from_utf8(&body).expect("utf8");
    assert!(
        body_str.contains("Subscription confirmed"),
        "expected confirmed page, got: {}",
        &body_str[..body_str.len().min(200)]
    );
    let sub = store
        .subscriber_get("ghost@example.com")
        .expect("read")
        .expect("subscriber");
    assert!(matches!(
        sub.state,
        epistole::store::SubscriberState::Active
    ));
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_javascript_url_in_markdown_is_sanitized() {
    // Audit findings #10 / #20: javascript: / data: URLs in markdown
    // links must be stripped (latent stored-XSS for Phase 2 archive).
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        Arc::clone(&store),
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

    let payload = r#"{"send_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","subject":"x","markdown":"[click](javascript:alert(1)) and [also](data:text/html,evil)"}"#;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("authorization", "Bearer operator-bearer-test")
                .header("x-forwarded-for", "203.0.113.53")
                .body(Body::from(payload.to_owned()))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    // Walk the sends partition; the most-recent record's body_html
    // must NOT contain javascript: or data: hrefs.
    let mut found_html: Option<String> = None;
    for entry in store.iter_sends().expect("iter") {
        let send = entry.expect("decode");
        found_html = Some(send.body_html);
    }
    let html = found_html.expect("at least one send");
    assert!(
        !html.contains("javascript:") && !html.contains("data:text"),
        "rendered body_html still contains a dangerous URL scheme: {html}"
    );
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
async fn unsubscribed_subscriber_cannot_resubscribe_to_reactivate_via_stale_token() {
    // Reaudit finding #23: re-subscribe path used to flip Unsubscribed
    // back to Pending, re-enabling a captured 24h confirm URL. Patch
    // refuses the re-subscribe state transition.
    use secrecy::ExposeSecret;

    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(Arc::clone(&store), Arc::clone(&cfg), test_mailer());

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let confirm_signed = epistole::token::sign(
        &epistole::token::Token::new(
            epistole::token::TokenKind::Confirm,
            "user@example.com".to_owned(),
            now + 3600,
            0,
        ),
        cfg.token_secret.expose_secret().as_bytes(),
    )
    .expect("sign");
    let unsub_signed = epistole::token::sign(
        &epistole::token::Token::new(
            epistole::token::TokenKind::Unsubscribe,
            "user@example.com".to_owned(),
            now + 3600,
            0,
        ),
        cfg.token_secret.expose_secret().as_bytes(),
    )
    .expect("sign");

    // 1. POST /subscribe
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/subscribe")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", "203.0.113.60")
                .body(Body::from("email=user%40example.com"))
                .expect("req"),
        )
        .await
        .expect("response");

    // 2. Confirm (creates Active)
    let _ = app
        .clone()
        .oneshot(post_form(
            "/confirm",
            "203.0.113.60",
            format!("token={confirm_signed}"),
        ))
        .await
        .expect("response");

    // 3. Unsubscribe (Active → Unsubscribed, bumps generation to 1)
    let _ = app
        .clone()
        .oneshot(post_form(
            "/unsubscribe",
            "203.0.113.60",
            format!("token={unsub_signed}"),
        ))
        .await
        .expect("response");

    // 4. Re-subscribe to the same address. With the patch this leaves
    //    state Unsubscribed (no flip back to Pending). Internally this
    //    mints a NEW confirm token at generation 1 (the row's current
    //    value) — a different token from confirm_signed above, and not
    //    exercised further by this test (that path is covered in
    //    tests/consent_contract.rs).
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/subscribe")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", "203.0.113.60")
                .body(Body::from("email=user%40example.com"))
                .expect("req"),
        )
        .await
        .expect("response");

    let sub_after_resub = store
        .subscriber_get("user@example.com")
        .expect("read")
        .expect("subscriber");
    assert!(
        matches!(
            sub_after_resub.state,
            epistole::store::SubscriberState::Unsubscribed
        ),
        "re-subscribe must NOT flip Unsubscribed → Pending (was {:?})",
        sub_after_resub.state
    );

    // 5. Replay the original confirm URL. Its generation (0) no longer
    //    matches the row's (1, bumped by the unsubscribe in step 3), so
    //    it stays refused independent of the Pending-flip fix this test
    //    was originally written against.
    let _ = app
        .clone()
        .oneshot(post_form(
            "/confirm",
            "203.0.113.60",
            format!("token={confirm_signed}"),
        ))
        .await
        .expect("response");

    let sub_final = store
        .subscriber_get("user@example.com")
        .expect("read")
        .expect("subscriber");
    assert!(
        matches!(
            sub_final.state,
            epistole::store::SubscriberState::Unsubscribed
        ),
        "captured confirm URL must NOT reactivate after Phase 1.5.1 fix (was {:?})",
        sub_final.state
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_unauth_does_not_parse_body() {
    // Audit finding #19: unauthenticated /send must not pay JSON-parse
    // cost. We verify by sending a body that WOULD fail JSON parsing
    // (so a 400 would be the parse-first path) and assert we got 401
    // instead. If the bearer compare runs first, we get 401 even on
    // garbage input.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("x-forwarded-for", "203.0.113.54")
                .body(Body::from("not-valid-json{{{"))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "auth must short-circuit BEFORE body parse on /send"
    );
}
