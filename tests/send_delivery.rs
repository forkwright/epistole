//! `POST /send` fan-out: per-recipient ledger rows, `send_id`
//! idempotency, and the hourly/daily send caps.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use epistole::store::{Subscriber, SubscriberState};
use epistole::{Config, SendId, Store, router};
use http_body_util::BodyExt;
use tempfile::TempDir;
use time::OffsetDateTime;
use tower::ServiceExt;

mod common;
use common::test_config;

/// [`test_config`] with the hourly/daily caps overridden - the default
/// 500/2000 is too generous to exercise a cap-refusal in a fast test.
fn capped_config(data_dir: std::path::PathBuf, hourly: u64, daily: u64) -> Config {
    Config {
        send_cap_per_hour: hourly,
        send_cap_per_day: daily,
        ..test_config(data_dir)
    }
}

fn put_active(store: &Store, email: &str) {
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
}

fn send_request(send_id: SendId, subject: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/send")
        .header("content-type", "application/json")
        .header("authorization", "Bearer operator-bearer-test")
        .body(Body::from(format!(
            "{{\"send_id\":\"{send_id}\",\"subject\":\"{subject}\",\"markdown\":\"# body\"}}"
        )))
        .expect("req")
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_id_replay_produces_no_additional_deliveries_or_sends() {
    // The idempotency contract: retrying the identical send_id must not
    // grow the mailer's send count and must not grow the deliveries
    // ledger. Asserting only on the HTTP response (e.g. "same send_id
    // came back") would pass even for a handler that silently re-sent
    // underneath and returned a look-alike reply - the mailer's own
    // counter is the only thing that actually observes the mechanism.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    put_active(&store, "reader-one@example.com");
    put_active(&store, "reader-two@example.com");
    let mailer = common::test_mailer();
    let app = router(
        Arc::clone(&store),
        Arc::new(test_config(tmp.path().to_path_buf())),
        Arc::clone(&mailer),
    );

    let send_id = SendId::generate();

    let first = app
        .clone()
        .oneshot(send_request(send_id, "Issue one"))
        .await
        .expect("response");
    assert_eq!(first.status(), StatusCode::OK);
    let first_json = json_body(first).await;
    assert_eq!(first_json["sent"], 2, "both recipients sent on first call");
    assert_eq!(first_json["already_delivered"], 0);
    assert_eq!(
        mailer.sent_count().await,
        2,
        "mailer must have actually been asked to send twice"
    );

    let second = app
        .oneshot(send_request(send_id, "Issue one"))
        .await
        .expect("response");
    assert_eq!(second.status(), StatusCode::OK);
    let second_json = json_body(second).await;
    assert_eq!(
        second_json["sent"], 0,
        "a replayed send_id must send to nobody new"
    );
    assert_eq!(
        second_json["already_delivered"], 2,
        "both recipients must be reported as already delivered"
    );
    assert_eq!(
        mailer.sent_count().await,
        2,
        "the mailer's send count must not grow across the replay - this is \
         the actual mechanism, not just a matching HTTP response"
    );

    // Zero additional ledger rows: still exactly one Delivery per
    // recipient, not two.
    for email in ["reader-one@example.com", "reader-two@example.com"] {
        let delivery = store
            .delivery_get(&send_id, email)
            .expect("delivery_get")
            .expect("delivery row exists");
        assert_eq!(delivery.status, epistole::store::DeliveryStatus::Sent);
    }
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_id_reused_with_different_content_is_refused() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    let send_id = SendId::generate();
    let first = app
        .clone()
        .oneshot(send_request(send_id, "Original subject"))
        .await
        .expect("response");
    assert_eq!(first.status(), StatusCode::OK);

    let second = app
        .oneshot(send_request(send_id, "A completely different subject"))
        .await
        .expect("response");
    assert_eq!(
        second.status(),
        StatusCode::BAD_REQUEST,
        "reusing a send_id for different content must be refused, not silently \
         accepted under either version"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_records_a_delivery_row_per_recipient() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    put_active(&store, "only-reader@example.com");
    let app = router(
        Arc::clone(&store),
        Arc::new(test_config(tmp.path().to_path_buf())),
        common::test_mailer(),
    );

    let send_id = SendId::generate();
    let resp = app
        .oneshot(send_request(send_id, "One recipient"))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let delivery = store
        .delivery_get(&send_id, "only-reader@example.com")
        .expect("delivery_get")
        .expect("row present");
    assert_eq!(delivery.send_id, send_id);
    assert_eq!(delivery.email, "only-reader@example.com");
    assert_eq!(delivery.status, epistole::store::DeliveryStatus::Sent);
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_defers_recipients_once_the_hourly_cap_is_reached() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    for i in 0..3 {
        put_active(&store, &format!("reader-{i}@example.com"));
    }
    // Cap of 1 - only one of the three recipients can be admitted.
    let app = router(
        Arc::clone(&store),
        Arc::new(capped_config(tmp.path().to_path_buf(), 1, 100)),
        common::test_mailer(),
    );

    let resp = app
        .oneshot(send_request(SendId::generate(), "Capped send"))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    assert_eq!(json["sent"], 1, "only one slot was under the hourly cap");
    assert_eq!(
        json["deferred_by_cap"], 2,
        "the other two recipients must be reported as deferred, not silently dropped"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn a_capped_send_id_is_resumable_once_the_window_resets() {
    // Not a real clock wait - a second call against a FRESH config with
    // headroom proves the earlier skip was "deferred", i.e. resumable
    // via the same send_id, not a silent drop. (The rolling-window
    // reset itself is store.rs's try_reserve_send_slot bucket tests.)
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    put_active(&store, "reader-a@example.com");
    put_active(&store, "reader-b@example.com");
    let mailer = common::test_mailer();

    let send_id = SendId::generate();
    let capped_app = router(
        Arc::clone(&store),
        Arc::new(capped_config(tmp.path().to_path_buf(), 1, 100)),
        Arc::clone(&mailer),
    );
    let first = capped_app
        .oneshot(send_request(send_id, "Resumable send"))
        .await
        .expect("response");
    let first_json = json_body(first).await;
    assert_eq!(first_json["sent"], 1);
    assert_eq!(first_json["deferred_by_cap"], 1);
    assert_eq!(mailer.sent_count().await, 1);

    // Same store, same send_id, uncapped config (simulating the window
    // having rolled over) - the recipient deferred above is picked up;
    // the one already sent is NOT re-sent.
    let uncapped_app = router(
        Arc::clone(&store),
        Arc::new(test_config(tmp.path().to_path_buf())),
        Arc::clone(&mailer),
    );
    let second = uncapped_app
        .oneshot(send_request(send_id, "Resumable send"))
        .await
        .expect("response");
    let second_json = json_body(second).await;
    assert_eq!(second_json["sent"], 1, "the deferred recipient is now sent");
    assert_eq!(
        second_json["already_delivered"], 1,
        "the previously-sent recipient must not be re-sent"
    );
    assert_eq!(
        mailer.sent_count().await,
        2,
        "exactly one new send happened - two recipients, two sends total, ever"
    );
}
