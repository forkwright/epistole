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
use crate::error::Result;
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
/// All "rejection" outcomes return the same `200 + invalid-link` page
/// shape, regardless of cause:
///   - bad signature
///   - expired token
///   - wrong kind (unsubscribe-token presented at /confirm)
///   - subscriber not found in store
///   - subscriber state is Unsubscribed (re-confirm refused — protects
///     CAN-SPAM/GDPR; once you opt out, a stale link can't bring you back)
///
/// This makes the response a membership-non-disclosure oracle: an
/// attacker holding a captured token can't learn whether the email
/// is still in the list.
///
/// # Errors
///
/// Returns [`Error::Store`] only — all other failures collapse into the
/// invalid-link page response.
pub(crate) async fn get(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<impl IntoResponse> {
    let invalid = || templates::invalid_token(&state.config.brand.name).into_response();

    let now = OffsetDateTime::now_utc();
    let token = match verify(
        &p.token,
        state.config.token_secret.expose_secret().as_bytes(),
        now.unix_timestamp(),
    ) {
        Ok(t) if t.kind == TokenKind::Confirm => t,
        _ => return Ok(invalid()),
    };

    let Some(mut subscriber) = state.store.subscriber_get(&token.email)? else {
        // Subscriber row missing — token references something that
        // doesn't exist. Emit the same shape as a bad token; never
        // 404, which would leak existence.
        return Ok(invalid());
    };

    match subscriber.state {
        SubscriberState::Unsubscribed => {
            // Stale confirm token cannot reactivate an unsubscribed
            // address. This is a hard refusal — once a visitor explicitly
            // opted out, only a fresh subscribe (which mints a new
            // token) can bring them back.
            Ok(invalid())
        }
        SubscriberState::Pending => {
            subscriber.state = SubscriberState::Active;
            subscriber.confirmed_at = Some(now);
            state.store.subscriber_put(&subscriber)?;
            Ok(templates::confirmed(&state.config.brand.name).into_response())
        }
        SubscriberState::Active => {
            // Idempotent confirm — already active. Return the success
            // page so re-clicking the email link doesn't surprise the
            // visitor.
            Ok(templates::confirmed(&state.config.brand.name).into_response())
        }
    }
}
