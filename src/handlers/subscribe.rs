//! `POST /subscribe` - accept an email and mint a stateless confirm token
//! without durably persisting unproven mailbox ownership.
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
/// Returns [`Error::BadRequest`] on a malformed or oversized email, or
/// [`Error::Config`] when token signing cannot be initialized.
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

    // Reaudit finding #23: subscribe handler MUST NOT flip an
    // Unsubscribed subscriber back to Pending. The previous code did,
    // re-enabling any captured confirm URL within its 24h TTL — a
    // two-step bypass of the #7/#18 fix in confirm.rs. Once a visitor
    // has explicitly opted out, confirm.rs still refuses to reactivate
    // them from a stale or newly minted confirm token.
    //
    // We respond with the same "pending" page shape regardless, so
    // an attacker probing membership cannot distinguish "already
    // unsubscribed" from "newly subscribed."
    //
    // Reaudit finding #26: short-circuiting Active/Unsubscribed used to
    // do less work than new/Pending paths. Confirm tokens are stateless
    // now, so every valid submit does the same token-signing work while
    // avoiding a fjall write until /confirm proves mailbox ownership.
    let now = OffsetDateTime::now_utc();

    let _signed = mint_confirm_token(&state, &email_norm, now)?;

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
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Sha256;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(state.config.token_secret.expose_secret().as_bytes())
                .map_err(|e| Error::Config {
                    reason: format!("HMAC key for log-hash: {e}"),
                })?;
        mac.update(email_norm.as_bytes());
        let digest = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(digest.len() * 2);
        for b in &digest {
            use std::fmt::Write;
            // WHY: write! to String is infallible; unwrap_or(()) is idiomatic.
            write!(hex, "{b:02x}").unwrap_or(());
        }
        hex[..16].to_owned()
    };
    tracing::info!(email_hmac_short = %email_hash, "confirm link minted (phase 0: operator mints URL out-of-band)");

    Ok(templates::pending(&state.config.brand.name, &email_norm).into_response())
}

/// Do the same CPU work for each valid subscribe path by minting the
/// stateless confirm token. The caller owns whether that token should be
/// mailed; Phase 0 only logs the HMAC'd address for operator correlation.
///
/// No subscriber row is written here. That is the security boundary for
/// forkwright/epistole#5: unconfirmed addresses never become durable
/// fjall state.
fn mint_confirm_token(state: &AppState, email_norm: &str, now: OffsetDateTime) -> Result<String> {
    let exp_unix = now.unix_timestamp() + CONFIRM_TTL_SECS;
    let token = crate::token::Token::new(
        crate::token::TokenKind::Confirm,
        email_norm.to_owned(),
        exp_unix,
    );
    crate::token::sign(&token, state.config.token_secret.expose_secret().as_bytes())
}
