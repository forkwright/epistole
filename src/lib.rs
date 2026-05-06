//! `epistole` - sovereign newsletter service.
//!
//! Public modules expose the router builder and storage primitives so
//! integration tests and out-of-process tools (e.g. `epistole-import`)
//! can reuse the same wiring as the running server.

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub(crate) mod handlers;
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
    GovernorLayer, governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor,
};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

pub use config::Config;
pub use error::{Error, Result};
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
pub fn router(store: Arc<Store>, config: Arc<Config>) -> Router {
    let state = AppState::new(store, config);

    // Per-IP rate limit. Conservative budget: 6 requests over 60 seconds,
    // bursts up to 6. Newsletter forms get one POST per visitor; legitimate
    // confirm/unsubscribe clicks are one GET. Nothing legitimate hits this
    // ceiling. Background: tower_governor's algorithm is a token bucket
    // backed by `governor` (lock-free), so the layer adds ~microseconds.
    //
    // SmartIpKeyExtractor honors X-Forwarded-For + X-Real-IP first, then
    // falls back to ConnectInfo. This is what we want behind NPM (the
    // reverse proxy sets X-Forwarded-For; without SmartIp every request
    // would key on 127.0.0.1 and the rate limit would be global).
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
            .key_extractor(SmartIpKeyExtractor)
            .finish()
            .expect("governor config valid"),
    );

    let public_routes = Router::new()
        .route("/subscribe", post(handlers::subscribe::post))
        .route("/confirm", get(handlers::confirm::get))
        .route("/unsubscribe", get(handlers::unsubscribe::get))
        .route("/archive", get(handlers::archive::get))
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
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .with_state(state)
}
