//! `GET /confirm?token=...` verifies the token and previews the outcome
//! without writing to the store; `POST /confirm` commits it.
//!
//! Splitting the surface this way keeps the safe method safe: a mail
//! client's link-prefetcher or a security scanner that fetches the GET
//! URL can no longer create or activate a subscriber on its own (RFC
//! 9110 §9.2.1, forkwright/epistole#68). The human's own click is what
//! submits the interstitial's form as the POST that actually confirms.

use axum::{
    Form,
    extract::{Query, State},
    http::header,
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

/// `Cache-Control` on every response this module renders. Each one
/// reflects live, per-visitor subscriber state derived from a
/// short-lived signed token; a shared cache or a browser back/forward
/// cache serving a stale copy would misrepresent that state to whoever
/// looks at the link next (forkwright/epistole#68).
const NO_STORE: &str = "no-store";

/// Query parameters for `GET /confirm`.
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

/// Form body for `POST /confirm`. Same shape as [`Params`]; kept as its
/// own type so the GET (query-string) and POST (form-body) extractors
/// stay independent of each other's evolution.
#[derive(Deserialize)]
pub(crate) struct Body {
    pub(crate) token: String,
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Body")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Handle `GET /confirm`. Performs the identical validation `POST
/// /confirm` will re-run, but only *reads* the subscriber row to choose
/// which page to render — never writes one. axum serves `HEAD /confirm`
/// from this same handler with the body dropped, so HEAD is covered by
/// the same no-write path with no extra code.
///
/// # Errors
///
/// Returns [`Error::Store`] only — all other failures collapse into the
/// invalid-link page response.
pub(crate) async fn get(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<impl IntoResponse> {
    let now = OffsetDateTime::now_utc();
    let verified = verify(
        &p.token,
        state.config.token_secret.expose_secret().as_bytes(),
        now.unix_timestamp(),
    )
    .ok()
    .filter(|t| t.kind == TokenKind::Confirm);

    let Some(token) = verified else {
        return Ok((
            [(header::CACHE_CONTROL, NO_STORE)],
            templates::invalid_token(&state.config.brand.name),
        ));
    };

    let subscriber = state.store.subscriber_get(&token.email)?;
    let page = match subscriber {
        // No row yet: a stateless confirm token proves the operator
        // minted a confirmation link, so a missing row previews as
        // eligible — POST would create it.
        None => templates::confirm_interstitial(&state.config.brand.name, &p.token),
        // Idempotent target state already reached; show the final page
        // directly rather than a button that would be a no-op.
        Some(s) if s.state == SubscriberState::Active => {
            templates::confirmed(&state.config.brand.name)
        }
        // Pending (legacy) or Unsubscribed, and this token's generation
        // still matches the row's current one — POST would activate it.
        Some(s) if s.generation == token.generation => {
            templates::confirm_interstitial(&state.config.brand.name, &p.token)
        }
        // Generation superseded by a later consent event — stale token.
        Some(_) => templates::invalid_token(&state.config.brand.name),
    };
    Ok(([(header::CACHE_CONTROL, NO_STORE)], page))
}

/// Handle `POST /confirm`. Re-verifies the token from scratch — never
/// trusts the `GET` preview, which ran as a separate request and could
/// be stale by the time the visitor submits the form.
///
/// # Errors
///
/// Returns [`Error::Store`] only — all other failures collapse into the
/// invalid-link page response.
pub(crate) async fn post(
    State(state): State<AppState>,
    Form(body): Form<Body>,
) -> Result<impl IntoResponse> {
    let now = OffsetDateTime::now_utc();
    let token = match verify(
        &body.token,
        state.config.token_secret.expose_secret().as_bytes(),
        now.unix_timestamp(),
    ) {
        Ok(t) if t.kind == TokenKind::Confirm => t,
        _ => {
            return Ok((
                [(header::CACHE_CONTROL, NO_STORE)],
                templates::invalid_token(&state.config.brand.name),
            ));
        }
    };

    let subscriber = state.store.subscriber_get(&token.email)?;

    let page = match subscriber {
        None => {
            // Confirm tokens are stateless: a valid signed token proves
            // the operator minted a confirmation link, so a missing
            // subscriber row is not an error. The durable Active row is
            // created only here, after proof, at the token's generation
            // (0 for a brand-new address — the same baseline
            // mint_confirm_token reads for one with no row yet).
            let subscriber = Subscriber {
                email: token.email,
                state: SubscriberState::Active,
                created_at: now,
                confirmed_at: Some(now),
                unsubscribed_at: None,
                generation: token.generation,
            };
            state.store.subscriber_put(&subscriber)?;
            templates::confirmed(&state.config.brand.name)
        }
        Some(subscriber) if subscriber.state == SubscriberState::Active => {
            // Idempotent — re-submitting an already-processed confirm
            // (double form-submit, browser retry) returns the same
            // success page without a second write. No generation check:
            // the transition already happened, so this arm can only
            // reproduce a no-op, never grant new authority.
            templates::confirmed(&state.config.brand.name)
        }
        Some(mut subscriber) if subscriber.generation == token.generation => {
            // Pending (legacy) or Unsubscribed, and this token was
            // minted at (and nothing has since moved past) the row's
            // current generation — forkwright/epistole#65's fresh case:
            // a re-subscribe minted after the last unsubscribe reads
            // the post-unsubscribe generation, so it lands here and is
            // honored, while the ORIGINAL pre-unsubscribe token carries
            // the stale value and falls to the arm below.
            //
            // WHY this arm does not bump generation: unsubscribe is the
            // operation whose bump is load-bearing (it is what makes a
            // pre-unsubscribe confirm token stale in the first place —
            // see unsubscribe.rs). A confirm success never needs to
            // invalidate anything by itself: any earlier-generation
            // token replayed after this either lands on the Active
            // idempotent arm above (harmless no-op) or was already
            // stale before this transition ran.
            subscriber.state = SubscriberState::Active;
            subscriber.confirmed_at = Some(now);
            state.store.subscriber_put(&subscriber)?;
            templates::confirmed(&state.config.brand.name)
        }
        Some(_) => {
            // Generation superseded — a later unsubscribe (or a fresher
            // confirm) moved the row on since this token was minted.
            templates::invalid_token(&state.config.brand.name)
        }
    };
    Ok(([(header::CACHE_CONTROL, NO_STORE)], page))
}
