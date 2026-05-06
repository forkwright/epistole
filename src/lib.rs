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

use axum::{
    Router,
    routing::{get, post},
};

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

/// Build the axum router. Exposed so integration tests can assert
/// against the same routing the production binary uses.
pub fn router(store: Arc<Store>, config: Arc<Config>) -> Router {
    let state = AppState::new(store, config);
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/subscribe", post(handlers::subscribe::post))
        .route("/confirm", get(handlers::confirm::get))
        .route("/unsubscribe", get(handlers::unsubscribe::get))
        .route("/archive", get(handlers::archive::get))
        .route("/send", post(handlers::send::post))
        .with_state(state)
}
