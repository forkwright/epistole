//! `POST /webhooks/delivery-events` - ledger update + suppression on
//! bounce/complaint.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use epistole::store::{Delivery, DeliveryStatus, Subscriber, SubscriberState};
use epistole::{SendId, Store, router};
use tempfile::TempDir;
use time::OffsetDateTime;
use tower::ServiceExt;

mod common;
use common::test_config;

fn seed(store: &Store, send_id: SendId, email: &str) {
    let now = OffsetDateTime::now_utc();
    store
        .subscriber_put(&Subscriber {
            email: email.to_owned(),
            state: SubscriberState::Active,
            created_at: now,
            confirmed_at: Some(now),
            unsubscribed_at: None,
            generation: 0,
        })
        .expect("subscriber_put");
    store
        .delivery_put(&Delivery {
            send_id,
            email: email.to_owned(),
            status: DeliveryStatus::Sent,
            at: now,
            error: None,
        })
        .expect("delivery_put");
}

fn webhook_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/webhooks/delivery-events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer webhook-bearer-test")
        .body(Body::from(body.to_owned()))
        .expect("req")
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn hard_bounce_updates_ledger_and_suppresses_the_subscriber() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let send_id = SendId::generate();
    seed(&store, send_id, "bounced@example.com");
    let app = router(
        Arc::clone(&store),
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    let resp = app
        .oneshot(webhook_request(&format!(
            "{{\"send_id\":\"{send_id}\",\"email\":\"bounced@example.com\",\"kind\":\"bounce\",\"hard\":true}}"
        )))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let delivery = store
        .delivery_get(&send_id, "bounced@example.com")
        .expect("delivery_get")
        .expect("row present");
    assert_eq!(delivery.status, DeliveryStatus::Bounced);

    let subscriber = store
        .subscriber_get("bounced@example.com")
        .expect("subscriber_get")
        .expect("row present");
    assert_eq!(
        subscriber.state,
        SubscriberState::Unsubscribed,
        "a hard bounce must suppress the subscriber"
    );
    assert_eq!(subscriber.generation, 1);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn soft_bounce_updates_ledger_but_does_not_suppress() {
    // Negative case for the suppression check above: a bounce with
    // hard=false (or omitted) must land the ledger row without touching
    // subscriber state - proving the suppression logic actually reads
    // the `hard` flag rather than suppressing on every bounce.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let send_id = SendId::generate();
    seed(&store, send_id, "soft-bounced@example.com");
    let app = router(
        Arc::clone(&store),
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    let resp = app
        .oneshot(webhook_request(&format!(
            "{{\"send_id\":\"{send_id}\",\"email\":\"soft-bounced@example.com\",\"kind\":\"bounce\"}}"
        )))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let delivery = store
        .delivery_get(&send_id, "soft-bounced@example.com")
        .expect("delivery_get")
        .expect("row present");
    assert_eq!(delivery.status, DeliveryStatus::Bounced);

    let subscriber = store
        .subscriber_get("soft-bounced@example.com")
        .expect("subscriber_get")
        .expect("row present");
    assert_eq!(
        subscriber.state,
        SubscriberState::Active,
        "a soft bounce (hard omitted -> false) must NOT suppress"
    );
    assert_eq!(subscriber.generation, 0);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn complaint_suppresses_regardless_of_hard() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let send_id = SendId::generate();
    seed(&store, send_id, "complained@example.com");
    let app = router(
        Arc::clone(&store),
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    let resp = app
        .oneshot(webhook_request(&format!(
            "{{\"send_id\":\"{send_id}\",\"email\":\"complained@example.com\",\"kind\":\"complaint\"}}"
        )))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let subscriber = store
        .subscriber_get("complained@example.com")
        .expect("subscriber_get")
        .expect("row present");
    assert_eq!(subscriber.state, SubscriberState::Unsubscribed);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn replaying_the_same_complaint_does_not_double_bump_generation() {
    // Idempotency for the state-modifying webhook: a relay retry after a
    // timeout (or a duplicate delivery of the same event) must not bump
    // generation a second time.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let send_id = SendId::generate();
    seed(&store, send_id, "double-complaint@example.com");
    let app = router(
        Arc::clone(&store),
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    let body = format!(
        "{{\"send_id\":\"{send_id}\",\"email\":\"double-complaint@example.com\",\"kind\":\"complaint\"}}"
    );
    for _ in 0..2 {
        let resp = app
            .clone()
            .oneshot(webhook_request(&body))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let subscriber = store
        .subscriber_get("double-complaint@example.com")
        .expect("subscriber_get")
        .expect("row present");
    assert_eq!(subscriber.state, SubscriberState::Unsubscribed);
    assert_eq!(
        subscriber.generation, 1,
        "replaying the same complaint must not bump generation twice"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn webhook_without_bearer_is_refused() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    let req = Request::builder()
        .method("POST")
        .uri("/webhooks/delivery-events")
        .header("content-type", "application/json")
        .body(Body::from(
            "{\"send_id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"email\":\"x@example.com\",\"kind\":\"bounce\"}",
        ))
        .expect("req");

    let resp = app.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn webhook_wrong_bearer_is_refused() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    // The operator's OWN send_auth_token must not also authenticate the
    // webhook route - the two bearer secrets are independent.
    let req = Request::builder()
        .method("POST")
        .uri("/webhooks/delivery-events")
        .header("content-type", "application/json")
        .header("authorization", "Bearer operator-bearer-test")
        .body(Body::from(
            "{\"send_id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"email\":\"x@example.com\",\"kind\":\"bounce\"}",
        ))
        .expect("req");

    let resp = app.oneshot(req).await.expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "send_auth_token must not double as the webhook secret"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn webhook_for_an_unrecorded_delivery_is_404() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    let resp = app
        .oneshot(webhook_request(
            "{\"send_id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"email\":\"nobody@example.com\",\"kind\":\"bounce\"}",
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
