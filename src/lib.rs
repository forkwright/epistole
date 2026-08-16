//! `epistole` - sovereign newsletter service.
//!
//! Public modules expose the router builder and storage primitives so
//! integration tests and out-of-process tools (e.g. `epistole-import`)
//! can reuse the same wiring as the running server.

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub(crate) mod handlers;
pub mod send_id;
pub mod store;
pub(crate) mod templates;
pub mod token;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, MatchedPath};
use axum::http::{Request, StatusCode};
use axum::{
    Router,
    routing::{get, post},
};
use tower_governor::{
    GovernorError, GovernorLayer, governor::GovernorConfigBuilder, key_extractor::KeyExtractor,
};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

pub use config::Config;
pub use error::{Error, Result};
pub use send_id::SendId;
pub use store::Store;

/// Shared application state passed to every handler.
#[derive(Clone)]
pub struct AppState {
    /// Persistence layer.
    pub(crate) store: Arc<Store>,
    /// Loaded configuration.
    pub(crate) config: Arc<Config>,
}

impl AppState {
    /// Construct a new `AppState`.
    #[must_use]
    pub fn new(store: Arc<Store>, config: Arc<Config>) -> Self {
        Self { store, config }
    }
}

/// Per-route body size cap (bytes). Public form posts (`/subscribe`)
/// only carry a tiny `email=` field; the operator endpoint (`/send`)
/// carries a markdown body — generous but capped well below memory
/// pressure. Defends against memory-DoS via large POSTs.
const SUBSCRIBE_BODY_LIMIT: usize = 4 * 1024; // 4 KiB
const SEND_BODY_LIMIT: usize = 256 * 1024; // 256 KiB
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Rate-limit key extractor that ONLY trusts the LAST entry of the
/// `X-Forwarded-For` header. Designed for deployment behind a single
/// trusted reverse proxy (NPM) that sets XFF to the real client IP via
/// `proxy_set_header X-Forwarded-For $remote_addr` (replace, not
/// append). In that topology, the last entry is the real client IP and
/// any earlier entries (which a hostile client can spoof) are ignored.
///
/// If XFF is missing entirely, fall back to `ConnectInfo` (the
/// immediate peer). This handles direct connections (smoke tests,
/// accidental bypass of NPM) without panicking.
#[derive(Clone, Copy, Debug)]
struct TrustedProxyExtractor;

impl KeyExtractor for TrustedProxyExtractor {
    type Key = std::net::IpAddr;

    fn extract<B>(
        &self,
        req: &axum::http::Request<B>,
    ) -> std::result::Result<Self::Key, GovernorError> {
        if let Some(xff) = req.headers().get("x-forwarded-for")
            && let Ok(s) = xff.to_str()
            && let Some(last) = s.rsplit(',').next()
            && let Ok(ip) = last.trim().parse::<std::net::IpAddr>()
        {
            return Ok(ip);
        }
        req.extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip())
            .ok_or(GovernorError::UnableToExtractKey)
    }
}

/// Builds the per-request tracing span. Replaces `TraceLayer`'s
/// `DefaultMakeSpan`, which records the complete `uri` — path AND query
/// string — as a span field: `tracing-subscriber`'s JSON formatter
/// attaches every currently-open span's fields to each event logged
/// inside it by default, so that one field alone put the raw uri on
/// every log line for the lifetime of the request. `GET /confirm`,
/// `GET /unsubscribe`, and `POST /unsubscribe/one-click` all carry a
/// signed capability token — and, nested inside it, the subscriber's
/// email — in that query string (forkwright/epistole#66).
///
/// Records only the method and the matched ROUTE TEMPLATE (e.g.
/// `/confirm`, never the literal request path or query). Falls back to
/// the bare request path when no route matched (a 404, where axum never
/// inserts `MatchedPath`) — `Uri::path()` never includes a query string
/// either, so the fallback carries the same guarantee.
fn make_span(request: &Request<Body>) -> tracing::Span {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or_else(|| request.uri().path());
    tracing::debug_span!("request", method = %request.method(), route = %route)
}

/// Build the axum router. Exposed so integration tests can assert
/// against the same routing the production binary uses.
///
/// Wiring order — outer to inner:
/// 1. tracing (every request logged; span carries method + matched route
///    only — see [`make_span`], forkwright/epistole#66)
/// 2. timeout (10s per request)
/// 3. per-route body limit (cheap rejection for oversized POSTs)
/// 4. per-IP rate limiter (defense against subscribe-flood / brute-force)
/// 5. handler
///
/// # Panics
///
/// Panics at startup if the hardcoded governor rate-limit configuration
/// is internally inconsistent (it is not; this is a defensive assertion).
pub fn router(store: Arc<Store>, config: Arc<Config>) -> Router {
    let state = AppState::new(store, config);

    // Per-IP rate limit. Conservative budget: 6 requests over 60 seconds,
    // bursts up to 6. Newsletter forms get one POST per visitor; legitimate
    // confirm/unsubscribe clicks are one GET. Nothing legitimate hits this
    // ceiling. Background: tower_governor's algorithm is a token bucket
    // backed by `governor` (lock-free), so the layer adds ~microseconds.
    //
    // Behind NPM, the reverse proxy ALWAYS sets X-Forwarded-For to a
    // single hop: the real client IP. The previous SmartIpKeyExtractor
    // implementation honored multi-hop XFF chains, which a hostile
    // client can spoof — set XFF to `1.2.3.4, 5.6.7.8, ...` and SmartIp
    // would key on `1.2.3.4`, defeating per-IP rate limiting.
    //
    // The TrustedProxyExtractor below ONLY accepts the LAST entry of
    // X-Forwarded-For (the value NPM most-recently appended). Combined
    // with NPM's `proxy_set_header X-Forwarded-For $remote_addr` config
    // (documented in DEPLOY.md step 8a), the chain is one hop and
    // unspoofable.
    //
    // The governor config has hardcoded values that always validate; the
    // unwrap below is defensive and triggers only on a typo at edit time.
    let governor_conf = Arc::new(
        #[expect(
            clippy::expect_used,
            reason = "config is hardcoded and constant; failure is a typo at edit time, not a runtime path"
        )]
        GovernorConfigBuilder::default()
            .per_second(10) // refill: 1 token per 10s -> 6 tokens / minute
            .burst_size(6)
            .key_extractor(TrustedProxyExtractor)
            .finish()
            .expect("governor config valid"),
    );

    let public_routes = Router::new()
        .route("/subscribe", post(handlers::subscribe::post))
        // GET previews without writing (RFC 9110 §9.2.1); POST commits.
        // See handlers/confirm.rs and handlers/unsubscribe.rs
        // (forkwright/epistole#68).
        .route(
            "/confirm",
            get(handlers::confirm::get).post(handlers::confirm::post),
        )
        .route(
            "/unsubscribe",
            get(handlers::unsubscribe::get).post(handlers::unsubscribe::post),
        )
        // RFC 8058 List-Unsubscribe-Post: a mail client POSTs here
        // directly, no interstitial. Deliberately a separate route from
        // the manual /unsubscribe above — see handlers/unsubscribe.rs.
        .route(
            "/unsubscribe/one-click",
            post(handlers::unsubscribe::one_click),
        )
        .route("/archive", get(handlers::archive::get))
        .route("/archive/{send_id}", get(handlers::archive::detail))
        .layer(GovernorLayer::new(governor_conf))
        .layer(DefaultBodyLimit::max(SUBSCRIBE_BODY_LIMIT))
        .layer(RequestBodyLimitLayer::new(SUBSCRIBE_BODY_LIMIT));

    // /send is operator-only; bearer auth gates abuse, no rate limit
    // (the operator hits it once per campaign). Body limit is larger
    // because the markdown body can be a full newsletter.
    let operator_routes = Router::new()
        .route("/send", post(handlers::send::post))
        .layer(DefaultBodyLimit::max(SEND_BODY_LIMIT))
        .layer(RequestBodyLimitLayer::new(SEND_BODY_LIMIT));

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(public_routes)
        .merge(operator_routes)
        .layer(TraceLayer::new_for_http().make_span_with(make_span))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tower_http::trace::{DefaultMakeSpan, MakeSpan};
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    /// In-memory sink for a `tracing_subscriber::fmt` writer, so a test
    /// can inspect the exact bytes a real JSON subscriber would have
    /// shipped to the journal — rather than trusting that a span-maker
    /// "looks right".
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

    /// Sets a real JSON `tracing_subscriber` as the thread-local default
    /// (wired the same way `main.rs` wires it — including
    /// `RUST_LOG=debug`, per forkwright/epistole#66's own reproduction
    /// steps, since `TraceLayer`'s span is created at `Level::DEBUG` and
    /// production's default filter is `info`), builds `span` UNDER that
    /// subscriber (a span built before its subscriber is active is
    /// permanently disabled — building it inside this closure, not
    /// passing an already-built `Span` in, is load-bearing), enters it,
    /// emits one probe event, and returns everything that subscriber
    /// wrote.
    fn captured_output(build_span: impl FnOnce() -> tracing::Span) -> String {
        let log = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(log.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let span = build_span();
        let _entered = span.enter();
        tracing::debug!("probe");
        drop(_entered);
        drop(span);
        log.as_string()
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn default_make_span_would_have_leaked_the_token_query_string() {
        // Negative-case fixture: proves `captured_output` actually
        // detects a leak, by reproducing one with the upstream default
        // `make_span` (above) replaces. If this assertion ever stops
        // holding, the positive test below is no longer trustworthy
        // either — the technique, not just the fix, is under test.
        let request = Request::builder()
            .uri("/confirm?token=PII_CAPABILITY_SENTINEL")
            .body(Body::empty())
            .expect("request");
        let out = captured_output(|| DefaultMakeSpan::new().make_span(&request));
        assert!(
            out.contains("PII_CAPABILITY_SENTINEL"),
            "expected DefaultMakeSpan to leak the token query string \
             (confidence check on the capture technique itself): {out}"
        );
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn make_span_never_records_the_query_string() {
        let request = Request::builder()
            .uri("/confirm?token=PII_CAPABILITY_SENTINEL")
            .body(Body::empty())
            .expect("request");
        let out = captured_output(|| make_span(&request));
        assert!(
            !out.contains("PII_CAPABILITY_SENTINEL"),
            "make_span must never record the query string: {out}"
        );
        assert!(
            out.contains("/confirm"),
            "make_span must still record the request path when no route \
             matched (the fallback branch, exercised here since a bare \
             Request::builder() carries no MatchedPath extension): {out}"
        );
    }
}
