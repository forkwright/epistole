//! `epistole` - sovereign newsletter service.
//!
//! Public modules expose the router builder and storage primitives so
//! integration tests and out-of-process tools (e.g. `epistole-import`)
//! can reuse the same wiring as the running server.

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub(crate) mod handlers;
pub mod mailer;
pub mod send_id;
pub mod store;
pub(crate) mod templates;
pub mod token;

use std::net::IpAddr;
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
pub use mailer::Mailer;
pub use send_id::SendId;
pub use store::Store;

/// Shared application state passed to every handler.
#[derive(Clone)]
pub struct AppState {
    /// Persistence layer.
    pub(crate) store: Arc<Store>,
    /// Loaded configuration.
    pub(crate) config: Arc<Config>,
    /// Outbound mail transport.
    pub(crate) mailer: Arc<dyn Mailer>,
}

impl AppState {
    /// Construct a new `AppState`.
    #[must_use]
    pub fn new(store: Arc<Store>, config: Arc<Config>, mailer: Arc<dyn Mailer>) -> Self {
        Self {
            store,
            config,
            mailer,
        }
    }
}

/// Per-route body size cap (bytes). Public form posts (`/subscribe`)
/// only carry a tiny `email=` field; the operator endpoint (`/send`)
/// carries a markdown body — generous but capped well below memory
/// pressure. Defends against memory-DoS via large POSTs.
const SUBSCRIBE_BODY_LIMIT: usize = 4 * 1024; // 4 KiB
const SEND_BODY_LIMIT: usize = 256 * 1024; // 256 KiB
/// Bounds a bounce/complaint webhook POST. A `{send_id, email, kind,
/// hard}` event is a few hundred bytes at most; 4 KiB matches
/// `SUBSCRIBE_BODY_LIMIT` and leaves generous headroom.
const WEBHOOK_BODY_LIMIT: usize = 4 * 1024; // 4 KiB
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Rate-limit key extractor that only honors `X-Forwarded-For` when the
/// immediate TCP peer (`ConnectInfo`) is a configured trusted proxy —
/// matched against `config::TrustedProxyRange`, so a `trusted_proxies`
/// entry may be a single address or a CIDR range. Any client reaching
/// epistole directly — bypassing the proxy, or because none is
/// configured — cannot spoof its rate-limit key by setting the header:
/// the header is ignored outright and the key is the verified peer
/// address instead. The peer is canonicalized (`IpAddr::to_canonical`)
/// before both the trust check and its use as a key, so a dual-stack
/// listener that surfaces an IPv4 proxy as `::ffff:a.b.c.d` still
/// matches a plain IPv4 `trusted_proxies` entry instead of silently
/// falling into the untrusted branch.
///
/// Single-hop model only: once the immediate peer is verified trusted,
/// the LAST `X-Forwarded-For` entry is taken at face value as the real
/// client (matching the deployed topology in `DEPLOY.md` step 8a, where
/// the one reverse proxy REPLACES rather than appends the header). A
/// chain of more than one trusted proxy — where an earlier hop's
/// address would need to be read out of the middle of the list — is
/// NOT handled; this extractor was not extended to walk an N-deep
/// trusted chain (forkwright/epistole#67 tracked both cases and scoped
/// the fix to the single-hop deployment that exists today).
///
/// If the peer is trusted but sends no `X-Forwarded-For` (misconfigured
/// proxy, or a probe that skipped it), the key falls back to the peer's
/// own address — a shared bucket, not a bypass. If `ConnectInfo` itself
/// is absent (should not happen in production; see
/// `into_make_service_with_connect_info` in `src/main.rs`), extraction
/// fails closed rather than trusting anything the client sent.
#[derive(Clone, Debug)]
struct TrustedProxyExtractor {
    trusted: Arc<[config::TrustedProxyRange]>,
}

impl TrustedProxyExtractor {
    fn new(trusted_proxies: &[config::TrustedProxyRange]) -> Self {
        Self {
            trusted: trusted_proxies.into(),
        }
    }

    fn is_trusted(&self, peer: IpAddr) -> bool {
        self.trusted.iter().any(|range| range.contains(peer))
    }
}

impl KeyExtractor for TrustedProxyExtractor {
    type Key = IpAddr;

    fn extract<B>(
        &self,
        req: &axum::http::Request<B>,
    ) -> std::result::Result<Self::Key, GovernorError> {
        let peer = req
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            // Canonicalize once here: an IPv4-mapped-IPv6 peer
            // (`::ffff:a.b.c.d`) on a dual-stack listener must match a
            // plain IPv4 `trusted_proxies` entry, and the untrusted
            // branch below returns this same value as the rate-limit
            // key, so the bucket stays stable regardless of which
            // socket-address shape the OS reported.
            .map(|ci| ci.0.ip().to_canonical())
            .ok_or(GovernorError::UnableToExtractKey)?;

        if !self.is_trusted(peer) {
            // Peer is not a configured trusted proxy. X-Forwarded-For is
            // fully client-controlled input from here on, so it MUST be
            // ignored entirely — trusting it is exactly the forgery this
            // extractor exists to close (forkwright/epistole#67).
            return Ok(peer);
        }

        if let Some(xff) = req.headers().get("x-forwarded-for")
            && let Ok(s) = xff.to_str()
            && let Some(last) = s.rsplit(',').next()
            && let Ok(ip) = last.trim().parse::<IpAddr>()
        {
            return Ok(ip);
        }
        Ok(peer)
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
        .map_or_else(|| request.uri().path(), MatchedPath::as_str);
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
pub fn router(store: Arc<Store>, config: Arc<Config>, mailer: Arc<dyn Mailer>) -> Router {
    let state = AppState::new(store, config, mailer);

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
    // TrustedProxyExtractor closes that AND the deeper hole it shared
    // with SmartIp: neither checked WHO was connecting. A request that
    // reaches epistole directly — no proxy in between at all — used to
    // have its X-Forwarded-For honored just the same, so any direct
    // client could forge its own rate-limit key (forkwright/epistole#67).
    // The extractor now only accepts the LAST X-Forwarded-For entry when
    // the immediate peer (ConnectInfo) is listed in `config.trusted_proxies`;
    // every other peer is keyed on its own verified address, header or
    // not. Combined with NPM's `proxy_set_header X-Forwarded-For
    // $remote_addr` config (documented in DEPLOY.md step 8a), the chain
    // is one verified hop and unspoofable.
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
            .key_extractor(TrustedProxyExtractor::new(&state.config.trusted_proxies))
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

    // The relay's bounce/complaint webhook - authenticated by its own
    // bearer token (config.webhook_auth_token), never the operator's
    // send_auth_token: a leaked webhook secret must not also authorize
    // triggering a send. No per-IP rate limit, matching /send - a bad
    // send can legitimately produce a burst of bounces the relay must
    // still be able to report in full.
    let webhook_routes = Router::new()
        .route("/webhooks/delivery-events", post(handlers::webhooks::post))
        .layer(DefaultBodyLimit::max(WEBHOOK_BODY_LIMIT))
        .layer(RequestBodyLimitLayer::new(WEBHOOK_BODY_LIMIT));

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(public_routes)
        .merge(operator_routes)
        .merge(webhook_routes)
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
