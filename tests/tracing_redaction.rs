//! Regression tests for forkwright/epistole#66: request tracing must
//! never record a capability token — or the subscriber email carried
//! inside it — from a query string.
//!
//! Drives real, signed tokens through the REAL production router
//! ([`router`]) with a real JSON `tracing_subscriber` wired as the
//! thread-local default, then asserts the captured output that
//! subscriber wrote does NOT contain the token, for all three
//! token-bearing routes: `GET /confirm`, `GET /unsubscribe`, and
//! `POST /unsubscribe/one-click`. This stays true even if the router's
//! `TraceLayer` is later reconfigured, because it asserts on the
//! observable output of the real span-maker rather than on which
//! function object is wired in.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use epistole::token::{Token, TokenKind, sign};
use epistole::{Store, router};
use secrecy::ExposeSecret;
use tempfile::TempDir;
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

mod common;
use common::{TRUSTED_PROXY_IP, test_config, test_mailer};

/// forkwright/epistole#67: `TrustedProxyExtractor` only honors
/// `X-Forwarded-For` from a peer listed in `trusted_proxies`, and fails
/// closed (500) with no verified peer at all — so every request built
/// here needs this stamped, matching the pattern in `tests/confirm_expiry.rs`
/// and friends, or the rate limiter (not the redaction this file tests)
/// rejects the request before a handler ever runs.
fn trusted_peer() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::new(TRUSTED_PROXY_IP, 0))
}

/// In-memory sink for a `tracing_subscriber::fmt` writer, so a test can
/// inspect the exact bytes a real subscriber would have shipped to the
/// journal.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn as_string(&self) -> String {
        String::from_utf8(self.0.lock().expect("lock").clone()).expect("utf8 log output")
    }
}

impl io::Write for CapturedLog {
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Sets a real JSON `tracing_subscriber` as the thread-local default —
/// `RUST_LOG=debug` equivalent, matching the issue's own reproduction
/// steps and the level `TraceLayer` creates its span at — drives `req`
/// through `app`, and returns everything that subscriber wrote alongside
/// the response status.
///
/// NOTE: no `#[expect(clippy::expect_used)]` here — `Router`'s `Service`
/// impl fixes `Error = Infallible`, so the one `.expect()` below unwraps
/// a `Result` clippy already knows can never be the `Err` arm, and does
/// not lint it; an `#[expect]` with nothing to suppress is itself an
/// error under `-D warnings` (`unfulfilled_lint_expectations`).
async fn drive_and_capture(app: axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let log = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(log.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let resp = app.oneshot(req).await.expect("response");
    let status = resp.status();
    (status, log.as_string())
}

/// A distinct, greppable local-part so a failed assertion's diff is
/// unambiguous about which fixture leaked.
const SENTINEL_EMAIL: &str = "pii-capability-sentinel@example.com";

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn confirm_get_trace_never_records_the_token() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(store, Arc::clone(&cfg), test_mailer());

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = Token::new(TokenKind::Confirm, SENTINEL_EMAIL.to_owned(), now + 3600, 0);
    let signed = sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    let (status, out) = drive_and_capture(
        app,
        Request::builder()
            .uri(format!("/confirm?token={signed}"))
            .header("x-forwarded-for", "203.0.113.210")
            .extension(trusted_peer())
            .body(Body::empty())
            .expect("req"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        !out.contains(&signed),
        "trace output must never contain the confirm token: {out}"
    );
    assert!(
        out.contains("/confirm"),
        "trace output must still record the matched route: {out}"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn unsubscribe_get_trace_never_records_the_token() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(store, Arc::clone(&cfg), test_mailer());

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = Token::new(
        TokenKind::Unsubscribe,
        SENTINEL_EMAIL.to_owned(),
        now + 3600,
        0,
    );
    let signed = sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    let (status, out) = drive_and_capture(
        app,
        Request::builder()
            .uri(format!("/unsubscribe?token={signed}"))
            .header("x-forwarded-for", "203.0.113.211")
            .extension(trusted_peer())
            .body(Body::empty())
            .expect("req"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        !out.contains(&signed),
        "trace output must never contain the unsubscribe token: {out}"
    );
    assert!(
        out.contains("/unsubscribe"),
        "trace output must still record the matched route: {out}"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn unsubscribe_one_click_trace_never_records_the_token() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let cfg = Arc::new(test_config(tmp.path().to_path_buf()));
    let app = router(store, Arc::clone(&cfg), test_mailer());

    // RFC 8058: the token rides the URL (the `List-Unsubscribe` mail
    // header carries it), while the POST body is the fixed one-click
    // marker — see handlers/unsubscribe.rs's module docs.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let tok = Token::new(
        TokenKind::Unsubscribe,
        SENTINEL_EMAIL.to_owned(),
        now + 3600,
        0,
    );
    let signed = sign(&tok, cfg.token_secret.expose_secret().as_bytes()).expect("sign");

    let (status, out) = drive_and_capture(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/unsubscribe/one-click?token={signed}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-forwarded-for", "203.0.113.212")
            .extension(trusted_peer())
            .body(Body::from("List-Unsubscribe=One-Click"))
            .expect("req"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        !out.contains(&signed),
        "trace output must never contain the one-click token: {out}"
    );
    assert!(
        out.contains("/unsubscribe/one-click"),
        "trace output must still record the matched route: {out}"
    );
}
