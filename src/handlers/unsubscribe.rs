//! Three unsubscribe entry points, all converging on the same
//! generation-checked transition:
//!
//! - `GET /unsubscribe?token=...` verifies the token and previews the
//!   outcome without writing to the store — safe against a mail-client
//!   prefetch or scanner (RFC 9110 §9.2.1, forkwright/epistole#68).
//! - `POST /unsubscribe` commits it. This is what the interstitial's
//!   own form submits after a human click.
//! - `POST /unsubscribe/one-click` is the RFC 8058 `List-Unsubscribe-Post`
//!   contract: a mail client POSTs here directly, no interstitial, body
//!   MUST be exactly `List-Unsubscribe=One-Click`. Kept as a distinct
//!   route from the manual path per forkwright/epistole#68's "separate
//!   manual unsubscribe from an RFC 8058 endpoint" — a human's browser
//!   never targets this URL, only a mail client acting on the
//!   `List-Unsubscribe` header a future Phase 2 send (forkwright/epistole#3)
//!   will set to this endpoint's URL.
//!
//! Idempotent: re-applying an unsubscribe (any of the three ways) to an
//! already-Unsubscribed address is a no-op state-wise and renders the
//! same success page.

use axum::{
    Form,
    extract::{Query, State},
    http::header,
    response::IntoResponse,
};
use maud::Markup;
use secrecy::ExposeSecret;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::AppState;
use crate::error::{Error, Result};
use crate::store::SubscriberState;
use crate::templates;
use crate::token::{TokenKind, verify};

/// `Cache-Control` on every response this module renders. See
/// `handlers/confirm.rs`'s identical constant for why.
const NO_STORE: &str = "no-store";

/// The exact RFC 8058 §3.1 one-click body value. Anything else is not a
/// legitimate one-click POST.
const ONE_CLICK_VALUE: &str = "One-Click";

/// Query parameters for `GET /unsubscribe` and the token half of
/// `POST /unsubscribe/one-click` (the `List-Unsubscribe` mail header
/// carries the token in the URL; the RFC 8058 body carries only the
/// fixed one-click marker, not the token).
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

/// Form body for `POST /unsubscribe`. Same shape as [`Params`]; kept as
/// its own type so the GET (query-string) and POST (form-body)
/// extractors stay independent of each other's evolution.
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

/// Form body for `POST /unsubscribe/one-click`. RFC 8058 §3.1 mandates
/// the message body be **exactly** `List-Unsubscribe=One-Click` — the
/// field name carries the hyphenated mail-header name verbatim, so it
/// needs an explicit rename; Rust identifiers can't contain `-`.
#[derive(Debug, Deserialize)]
pub(crate) struct OneClickBody {
    #[serde(rename = "List-Unsubscribe")]
    pub(crate) list_unsubscribe: String,
}

/// Handle `GET /unsubscribe`. Performs the identical validation `POST
/// /unsubscribe` will re-run, but only *reads* the subscriber row to
/// choose which page to render — never writes one. axum serves
/// `HEAD /unsubscribe` from this same handler with the body dropped, so
/// HEAD is covered by the same no-write path with no extra code.
///
/// All rejection paths collapse to the same `200 + invalid-link` shape
/// (membership-non-disclosure); the success path is the unsubscribed
/// page or, when a transition is still pending, the interstitial.
///
/// # Errors
///
/// Returns [`Error::Store`] only — all other failures collapse into the
/// invalid-link page.
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
    .filter(|t| t.kind == TokenKind::Unsubscribe);

    let Some(token) = verified else {
        return Ok((
            [(header::CACHE_CONTROL, NO_STORE)],
            templates::invalid_token(&state.config.brand.name),
        ));
    };

    let subscriber = state.store.subscriber_get(&token.email)?;
    let page = match subscriber {
        // Idempotent target state already reached; show the final page
        // directly rather than a button that would be a no-op.
        Some(s) if s.state == SubscriberState::Unsubscribed => {
            templates::unsubscribed(&state.config.brand.name)
        }
        // Active, and this token's generation still matches the row's
        // current one — POST would unsubscribe it. Otherwise a later
        // consent event (a later opt-in cycle) superseded it,
        // forkwright/epistole#65 — same invalid-link shape as "nothing
        // to unsubscribe" below.
        Some(s) if s.generation == token.generation => {
            templates::unsubscribe_interstitial(&state.config.brand.name, &p.token)
        }
        // Either nothing to unsubscribe (None) or a stale generation
        // (Some) — one arm covers both since the page is identical
        // either way (membership-non-disclosure).
        _ => templates::invalid_token(&state.config.brand.name),
    };
    Ok(([(header::CACHE_CONTROL, NO_STORE)], page))
}

/// Handle `POST /unsubscribe`. Re-verifies the token from scratch —
/// never trusts the `GET` preview, which ran as a separate request and
/// could be stale by the time the visitor submits the form.
///
/// # Errors
///
/// Returns [`Error::Store`] only — all other failures collapse into the
/// invalid-link page.
pub(crate) async fn post(
    State(state): State<AppState>,
    Form(body): Form<Body>,
) -> Result<impl IntoResponse> {
    let now = OffsetDateTime::now_utc();
    let page = apply(&state, &body.token, now)?;
    Ok(([(header::CACHE_CONTROL, NO_STORE)], page))
}

/// Handle `POST /unsubscribe/one-click` — the RFC 8058
/// `List-Unsubscribe-Post` contract. No interstitial: the RFC requires
/// this action complete on the single POST with no further interaction,
/// so it goes straight to [`apply`] the same way [`post`] does after its
/// form submit.
///
/// # Errors
///
/// Returns [`Error::BadRequest`] when the body is not exactly
/// `List-Unsubscribe=One-Click`. Returns [`Error::Store`] on a store
/// failure; all other token failures collapse into the invalid-link
/// page (still `200`, since a mail client does not render an error page
/// and a non-`2xx` here would just make it retry against a token that
/// will never become valid).
pub(crate) async fn one_click(
    State(state): State<AppState>,
    Query(p): Query<Params>,
    Form(body): Form<OneClickBody>,
) -> Result<impl IntoResponse> {
    if body.list_unsubscribe != ONE_CLICK_VALUE {
        return Err(Error::BadRequest {
            reason: format!(
                "List-Unsubscribe-Post body must be exactly \
                 'List-Unsubscribe={ONE_CLICK_VALUE}' per RFC 8058 §3.1"
            ),
        });
    }
    let now = OffsetDateTime::now_utc();
    let page = apply(&state, &p.token, now)?;
    // Status defaults to 200, matching every other handler in this
    // module — a mail client does not render the body either way.
    Ok(([(header::CACHE_CONTROL, NO_STORE)], page))
}

/// Verify an unsubscribe `token` and apply the generation-checked
/// transition if eligible. Shared by [`post`] (visitor-driven, via the
/// interstitial's form) and [`one_click`] (RFC 8058, mail-client-driven)
/// so the two commit paths can never diverge on what counts as a valid
/// transition.
fn apply(state: &AppState, raw_token: &str, now: OffsetDateTime) -> Result<Markup> {
    let token = match verify(
        raw_token,
        state.config.token_secret.expose_secret().as_bytes(),
        now.unix_timestamp(),
    ) {
        Ok(t) if t.kind == TokenKind::Unsubscribe => t,
        _ => return Ok(templates::invalid_token(&state.config.brand.name)),
    };

    let subscriber = state.store.subscriber_get(&token.email)?;
    match subscriber {
        Some(subscriber) if subscriber.state == SubscriberState::Unsubscribed => {
            // Idempotent — no write. No generation check: the
            // transition already happened, so this arm can only
            // reproduce a no-op, never grant new authority.
            Ok(templates::unsubscribed(&state.config.brand.name))
        }
        Some(mut subscriber) if subscriber.generation == token.generation => {
            // Active, and this token was minted at (and nothing has
            // since moved past) the row's current generation.
            //
            // WHY this arm bumps generation and confirm's analogous arm
            // does not: this bump is what makes a captured
            // pre-unsubscribe confirm token stale (forkwright/epistole#65's
            // classic case — see confirm.rs), and what makes an old
            // unsubscribe token replayed after a later resubscribe+
            // reconfirm cycle unable to cancel that later opt-in: the
            // old token's generation can no longer match a row this
            // transition has already moved past.
            subscriber.state = SubscriberState::Unsubscribed;
            subscriber.unsubscribed_at = Some(now);
            subscriber.generation += 1;
            state.store.subscriber_put(&subscriber)?;
            Ok(templates::unsubscribed(&state.config.brand.name))
        }
        // No row (nothing to unsubscribe) or a stale generation — one
        // arm covers both, since the page is identical either way
        // (membership-non-disclosure).
        _ => Ok(templates::invalid_token(&state.config.brand.name)),
    }
}
