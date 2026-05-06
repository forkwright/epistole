//! Error type for `epistole`. Single enum; variants carry context so
//! the JSON tracing log can attribute every failure.

use std::result::Result as StdResult;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use snafu::Snafu;

/// `Result` alias used throughout the crate.
pub type Result<T> = StdResult<T, Error>;

/// All errors `epistole` can emit. `IntoResponse` is implemented so
/// handler errors map cleanly to HTTP responses without a per-handler
/// match.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
#[non_exhaustive]
pub enum Error {
    /// Failed to load or parse `epistole.toml`.
    #[snafu(display("config error: {reason}"))]
    Config {
        /// Human-readable reason; included in the log line, never exposed via HTTP.
        reason: String,
    },

    /// Failed to bind the TCP listener.
    #[snafu(display("bind {addr}: {source}"))]
    Bind {
        /// Bind address that failed.
        addr: String,
        /// Underlying io error from the kernel.
        source: std::io::Error,
    },

    /// `axum::serve` returned an error.
    #[snafu(display("serve: {source}"))]
    Serve {
        /// Underlying io error from the runtime.
        source: std::io::Error,
    },

    /// Storage layer (fjall) failure.
    #[snafu(display("store: {reason}"))]
    Store {
        /// Human-readable reason; included in the log line, never exposed via HTTP.
        reason: String,
    },

    /// Bad request - caller sent malformed input.
    #[snafu(display("bad request: {reason}"))]
    BadRequest {
        /// Reason surfaced to the caller in the 400 body.
        reason: String,
    },

    /// Authentication failure on `/send`.
    #[snafu(display("unauthorized"))]
    Unauthorized,

    /// Token verification failed (bad signature, expired, unknown subscriber).
    #[snafu(display("invalid token"))]
    InvalidToken,

    /// Subscriber lookup miss.
    #[snafu(display("not found"))]
    NotFound,

    /// SMTP relay failure.
    #[snafu(display("smtp: {reason}"))]
    Smtp {
        /// Human-readable reason; included in the log line, never exposed via HTTP.
        reason: String,
    },
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            Self::BadRequest { reason } => (StatusCode::BAD_REQUEST, reason.clone()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_owned()),
            Self::InvalidToken => (StatusCode::BAD_REQUEST, "invalid token".to_owned()),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_owned()),
            // 5xx - log the server-side detail, surface a generic message
            // so we don't leak internals to callers.
            err => {
                tracing::error!(error = %err, "server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_owned(),
                )
            }
        };
        (status, body).into_response()
    }
}
