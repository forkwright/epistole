//! `POST /subscribe` - accept an email; create or refresh a Pending
//! subscriber; mint a confirm token; mail the confirm link.
//!
//! Idempotent at the data level: re-submitting an already-active email
//! short-circuits to a "you're already subscribed" page (treating it as
//! success keeps the form flow honest).

use axum::{Form, extract::State, response::IntoResponse};
use email_address::{EmailAddress, Options};
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

/// Maximum length we'll accept for an email address. RFC 5321 says
/// path = 256 octets including the angle brackets, leaving 254 for the
/// addr. Anything longer is either an attack or a typo; reject early.
const MAX_EMAIL_LEN: usize = 254;

/// Strict email validator options. The default `Options` accepts the
/// full RFC 5322 mailbox form including `Display Name <addr@host>`,
/// which would let an attacker submit `"Pwned <victim@example.com>"`
/// and mail the confirmation link to a victim. The strict options below
/// only accept the bare addr-spec form.
fn strict_email_options() -> Options {
    Options::default()
        .without_display_text()
        .with_required_tld()
        .without_domain_literal()
}

/// Handle `POST /subscribe`.
///
/// # Errors
///
/// Returns [`Error::BadRequest`] on a malformed or oversized email,
/// [`Error::Store`] on a fjall failure, [`Error::Smtp`] when the relay
/// refuses the confirmation message.
pub(crate) async fn post(
    State(state): State<AppState>,
    Form(body): Form<Body>,
) -> Result<impl IntoResponse> {
    let email_norm = body.email.trim().to_ascii_lowercase();
    if email_norm.is_empty() || email_norm.len() > MAX_EMAIL_LEN {
        return Err(Error::BadRequest {
            reason: "invalid email address".to_owned(),
        });
    }
    if EmailAddress::parse_with_options(&email_norm, strict_email_options()).is_err() {
        return Err(Error::BadRequest {
            reason: "invalid email address".to_owned(),
        });
    }

    let now = OffsetDateTime::now_utc();
    let existing = state.store.subscriber_get(&email_norm)?;

    // Reaudit finding #23: subscribe handler MUST NOT flip an
    // Unsubscribed subscriber back to Pending. The previous code did,
    // re-enabling any captured confirm URL within its 24h TTL — a
    // two-step bypass of the #7/#18 fix in confirm.rs. Once a visitor
    // has explicitly opted out, only the operator (or a future
    // out-of-band re-consent flow) can bring them back.
    //
    // We respond with the same "pending" page shape regardless, so
    // an attacker probing membership cannot distinguish "already
    // unsubscribed" from "newly subscribed."
    //
    // Reaudit finding #26: the Active and Unsubscribed branches both
    // do less work than the Pending/None branches (no token mint, no
    // fjall write), creating a timing oracle. We equalize by performing
    // a same-shape token mint + a NO-OP fjall write on the
    // short-circuit branches before returning the pending page.
    let subscriber = match existing {
        Some(s) if s.state == SubscriberState::Active => {
            // Equalize: mint a token (discarded) and re-put the
            // subscriber unchanged. Same CPU + I/O as the pending path.
            timing_equalize(&state, &email_norm, &s, now)?;
            return Ok(templates::pending(&state.config.brand.name, &email_norm).into_response());
        }
        Some(s) if s.state == SubscriberState::Unsubscribed => {
            // Same equalization. Subscriber stays Unsubscribed.
            timing_equalize(&state, &email_norm, &s, now)?;
            return Ok(templates::pending(&state.config.brand.name, &email_norm).into_response());
        }
        Some(mut s) => {
            // Pending refresh — extends the window and resets timestamps.
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
    let _signed = crate::token::sign(&token, state.config.token_secret.expose_secret().as_bytes())?;

    // Phase 2 (forkwright/epistole#1) wires lettre — until then, the
    // operator pulls the confirm URL by signing it themselves with
    // `epistole-mint-token` (or a manual sign() call). The log line
    // intentionally does NOT include the email or the confirm URL:
    // both are token-bearing PII that flows into Vector → GreptimeDB
    // and journal logs may persist for weeks. A hash digest of the
    // email gives the operator just enough to correlate without
    // leaking the address.
    // Reaudit finding #28: hash the email with the token_secret as
    // an HMAC key so a journal exfil can't be rainbow-matched against
    // a known target list. The 16-hex truncation gives the operator
    // enough entropy to correlate (64 bits — collision-free at any
    // realistic subscriber count) without leaking the address.
    let email_hash = {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(state.config.token_secret.expose_secret().as_bytes())
                .map_err(|e| Error::Config {
                    reason: format!("HMAC key for log-hash: {e}"),
                })?;
        mac.update(email_norm.as_bytes());
        let digest = mac.finalize().into_bytes();
        format!("{digest:x}")[..16].to_owned()
    };
    tracing::info!(email_hmac_short = %email_hash, "confirm link minted (phase 0: operator mints URL out-of-band)");

    Ok(templates::pending(&state.config.brand.name, &email_norm).into_response())
}

/// Do the same CPU + I/O work the Pending path does, on the
/// short-circuit (Active / Unsubscribed) branches — defends against
/// timing-oracle membership disclosure (reaudit finding #26).
///
/// Mints a Confirm token over the same secret (discarded), and re-puts
/// the subscriber record unchanged. The fjall write is idempotent
/// when the value is byte-identical, so the LSM-tree compaction sees
/// no growth from this call.
fn timing_equalize(
    state: &AppState,
    email_norm: &str,
    subscriber: &Subscriber,
    now: OffsetDateTime,
) -> Result<()> {
    let exp_unix = now.unix_timestamp() + CONFIRM_TTL_SECS;
    let token = crate::token::Token::new(
        crate::token::TokenKind::Confirm,
        email_norm.to_owned(),
        exp_unix,
    );
    let _ = crate::token::sign(&token, state.config.token_secret.expose_secret().as_bytes())?;
    state.store.subscriber_put(subscriber)?;
    Ok(())
}
