//! Resource bounds on the operator-only `POST /send` surface.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use epistole::{SendId, Store, router};
use tempfile::TempDir;
use tower::ServiceExt;

mod common;
use common::{test_config, test_mailer};

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn send_bounds_the_subject_length_at_the_cap() {
    // Issue #44: send.subject had no maximum length, so a single send
    // could store an arbitrarily long string and render it into the
    // archive <title> and <h1>. Asserted as a boundary pair so the test
    // fails if the bound is removed AND if it is set to reject
    // everything.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(
        store,
        Arc::new(test_config(tmp.path().to_path_buf())),
        test_mailer(),
    );

    // 201 bytes — one past the cap — is refused.
    let over = "a".repeat(201);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("authorization", "Bearer operator-bearer-test")
                .body(Body::from(format!(
                    "{{\"send_id\":\"{}\",\"subject\":\"{over}\",\"markdown\":\"# body\"}}",
                    SendId::generate()
                )))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a subject past the cap must be refused"
    );

    // 200 bytes — exactly the cap — is accepted.
    let at_limit = "a".repeat(200);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .header("content-type", "application/json")
                .header("authorization", "Bearer operator-bearer-test")
                .body(Body::from(format!(
                    "{{\"send_id\":\"{}\",\"subject\":\"{at_limit}\",\"markdown\":\"# body\"}}",
                    SendId::generate()
                )))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a subject at exactly the cap must be accepted"
    );
}
