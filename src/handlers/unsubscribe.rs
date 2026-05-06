//! `GET /unsubscribe?token=...` - verify the token, flip the subscriber
//! to Unsubscribed, render a success page.
//!
//! Idempotent: re-clicking an unsubscribe link is a no-op state-wise
//! and renders the same success page.

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use secrecy::ExposeSecret;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::AppState;
use crate::error::{Error, Result};
use crate::store::SubscriberState;
use crate::templates;
use crate::token::{TokenKind, verify};

/// Query parameters for `/unsubscribe`.
#[derive(Deserialize)]
pub(crate) struct Params {
    pub(crate) token: String,
}

impl std::fmt::Debug for Params {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Params")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Handle `GET /unsubscribe`.
///
/// # Errors
///
/// Same shape as `confirm::get` - invalid tokens render an expired-link
/// page; everything else propagates.
pub(crate) async fn get(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<impl IntoResponse> {
    let now = OffsetDateTime::now_utc();
    let token = match verify(
        &p.token,
        state.config.token_secret.expose_secret().as_bytes(),
        now.unix_timestamp(),
    ) {
        Ok(t) if t.kind == TokenKind::Unsubscribe => t,
        _ => return Ok(templates::invalid_token(&state.config.brand.name).into_response()),
    };

    let mut subscriber = state
        .store
        .subscriber_get(&token.email)?
        .ok_or(Error::NotFound)?;
    if subscriber.state != SubscriberState::Unsubscribed {
        subscriber.state = SubscriberState::Unsubscribed;
        subscriber.unsubscribed_at = Some(now);
        state.store.subscriber_put(&subscriber)?;
    }
    Ok(templates::unsubscribed(&state.config.brand.name).into_response())
}
