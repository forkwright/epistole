//! `POST /subscribe` - accept an email; create or refresh a Pending
//! subscriber; mint a confirm token; mail the confirm link.
//!
//! Idempotent at the data level: re-submitting an already-active email
//! short-circuits to a "you're already subscribed" page (treating it as
//! success keeps the form flow honest).

use axum::{Form, extract::State, response::IntoResponse};
use email_address::EmailAddress;
use secrecy::ExposeSecret;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::AppState;
use crate::error::{Error, Result};
use crate::store::{Subscriber, SubscriberState};
use crate::templates;

/// Form body for the subscribe endpoint.
#[derive(Debug, Deserialize)]
pub(crate) struct Body {
    pub(crate) email: String,
}

/// Token TTL for the confirm link - 24 hours.
const CONFIRM_TTL_SECS: i64 = 24 * 3600;

/// Handle `POST /subscribe`.
///
/// # Errors
///
/// Returns [`Error::BadRequest`] on a malformed email, [`Error::Store`]
/// on a fjall failure, [`Error::Smtp`] when the relay refuses the
/// confirmation message.
pub(crate) async fn post(
    State(state): State<AppState>,
    Form(body): Form<Body>,
) -> Result<impl IntoResponse> {
    let email_norm = body.email.trim().to_ascii_lowercase();
    if !EmailAddress::is_valid(&email_norm) {
        return Err(Error::BadRequest {
            reason: "invalid email address".to_owned(),
        });
    }

    let now = OffsetDateTime::now_utc();
    let existing = state.store.subscriber_get(&email_norm)?;
    let subscriber = match existing {
        Some(s) if s.state == SubscriberState::Active => {
            // Already subscribed - show pending page anyway (no leak about
            // membership state).
            return Ok(templates::pending(&state.config.brand.name, &email_norm).into_response());
        }
        Some(mut s) => {
            s.state = SubscriberState::Pending;
            s.created_at = now;
            s.confirmed_at = None;
            s.unsubscribed_at = None;
            s
        }
        None => Subscriber {
            email: email_norm.clone(),
            state: SubscriberState::Pending,
            created_at: now,
            confirmed_at: None,
            unsubscribed_at: None,
        },
    };
    state.store.subscriber_put(&subscriber)?;

    let exp_unix = now.unix_timestamp() + CONFIRM_TTL_SECS;
    let token = crate::token::Token::new(
        crate::token::TokenKind::Confirm,
        email_norm.clone(),
        exp_unix,
    );
    let signed = crate::token::sign(&token, state.config.token_secret.expose_secret().as_bytes())?;
    let confirm_url = format!("{}/confirm?token={signed}", state.config.base_url);

    // TODO(forkwright/epistole#1): wire lettre relay here. For now we log
    // the confirm URL so smoke tests can verify the flow without hitting
    // an outbound SMTP server.
    tracing::info!(email = %email_norm, confirm_url = %confirm_url, "confirm link minted");

    Ok(templates::pending(&state.config.brand.name, &email_norm).into_response())
}
