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

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
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

/// Build the axum router. Exposed so integration tests can assert
/// against the same routing the production binary uses.
///
/// Wiring order — outer to inner:
/// 1. tracing (every request logged)
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
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .with_state(state)
}
