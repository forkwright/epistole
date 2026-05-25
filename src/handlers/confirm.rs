//! `GET /confirm?token=...` - verify the stateless token, create or flip
//! the subscriber to Active, render a success page.

use axum::{
    extract::{Query, State},
    response::IntoResponse,
};
use secrecy::ExposeSecret;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::AppState;
use crate::error::Result;
use crate::store::{Subscriber, SubscriberState};
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
///   - subscriber state is Unsubscribed (re-confirm refused — protects
///     CAN-SPAM/GDPR; once you opt out, a stale link can't bring you back)
///
/// Confirm tokens are stateless: a valid signed token proves the operator
/// minted a confirmation link, so a missing subscriber row is not an error.
/// The handler creates the durable `Active` row only after this proof step.
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

    let subscriber = state.store.subscriber_get(&token.email)?;

    match subscriber {
        Some(subscriber) if subscriber.state == SubscriberState::Unsubscribed => {
            // Stale confirm token cannot reactivate an unsubscribed
            // address. This is a hard refusal — once a visitor explicitly
            // opted out, only a fresh subscribe (which mints a new
            // token) can bring them back.
            Ok(invalid())
        }
        Some(mut subscriber) if subscriber.state == SubscriberState::Pending => {
            // Legacy pre-#5 pending rows still confirm cleanly, but the
            // subscribe path no longer creates or refreshes them.
            subscriber.state = SubscriberState::Active;
            subscriber.confirmed_at = Some(now);
            state.store.subscriber_put(&subscriber)?;
            Ok(templates::confirmed(&state.config.brand.name).into_response())
        }
        Some(subscriber) if subscriber.state == SubscriberState::Active => {
            // Idempotent confirm — already active. Return the success
            // page so re-clicking the email link doesn't surprise the
            // visitor.
            Ok(templates::confirmed(&state.config.brand.name).into_response())
        }
        Some(_) => Ok(invalid()),
        None => {
            let subscriber = Subscriber {
                email: token.email,
                state: SubscriberState::Active,
                created_at: now,
                confirmed_at: Some(now),
                unsubscribed_at: None,
            };
            state.store.subscriber_put(&subscriber)?;
            Ok(templates::confirmed(&state.config.brand.name).into_response())
        }
    }
}
