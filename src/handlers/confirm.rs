//! `GET /confirm?token=...` - verify the token, flip the subscriber to
//! Active, render a success page.

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

/// Query parameters for `/confirm`.
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

/// Handle `GET /confirm`.
///
/// # Errors
///
/// Maps [`Error::InvalidToken`] to a friendly expired-link page (still
/// 200, since we don't want browsers caching a 4xx that fades after the
/// token would have expired anyway). Other errors propagate.
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
        Ok(t) if t.kind == TokenKind::Confirm => t,
        _ => return Ok(templates::invalid_token(&state.config.brand.name).into_response()),
    };

    let mut subscriber = state
        .store
        .subscriber_get(&token.email)?
        .ok_or(Error::NotFound)?;
    if subscriber.state != SubscriberState::Active {
        subscriber.state = SubscriberState::Active;
        subscriber.confirmed_at = Some(now);
        state.store.subscriber_put(&subscriber)?;
    }
    Ok(templates::confirmed(&state.config.brand.name).into_response())
}
