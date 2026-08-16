//! `POST /webhooks/delivery-events` - the relay's bounce/complaint
//! callback. Auth via `Authorization: Bearer ...` matching
//! `config.webhook_auth_token`.
//!
//! The payload contract is deliberately provider-agnostic
//! (`{send_id, email, kind, hard}`), not Postmark's or Mailgun's native
//! webhook JSON shape: picking one of those is a Phase 3 operator
//! decision (which relay account, real DNS/DKIM/domain) this crate
//! doesn't make. What Phase 2 owns is the ledger update and the
//! suppression rule once ANY relay's webhook (proxied or adapted into
//! this shape) reports one of these two outcomes for a delivery this
//! server actually recorded.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use secrecy::ExposeSecret;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::AppState;
use crate::error::{Error, Result};
use crate::handlers::check_bearer;
use crate::send_id::SendId;
use crate::store::{DeliveryStatus, SubscriberState};

/// Hard cap on webhook request bodies. Mirrors the router-level
/// `RequestBodyLimitLayer` (`WEBHOOK_BODY_LIMIT` in `lib.rs`) so the body
/// collector here can't be tricked by a stripped layer — same
/// defense-in-depth reasoning as `handlers/send.rs`'s identical
/// duplication.
const WEBHOOK_BODY_LIMIT_BYTES: usize = 4 * 1024;

/// What the relay is reporting.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EventKind {
    /// The message could not be delivered.
    Bounce,
    /// The recipient marked the message as spam.
    Complaint,
}

/// Request body for `POST /webhooks/delivery-events`.
#[derive(Deserialize)]
pub(crate) struct Body {
    pub(crate) send_id: SendId,
    pub(crate) email: String,
    pub(crate) kind: EventKind,
    /// Only meaningful when `kind == Bounce`. A soft bounce (mailbox
    /// full, greylisted, transient relay error) is retryable and does
    /// NOT suppress the subscriber; a hard bounce (no such mailbox,
    /// domain doesn't exist) means every future send to this address
    /// will fail the same way, so it suppresses exactly like a
    /// complaint. Defaults to `false` (soft) — an event source that
    /// omits this field is treated as non-suppressing, the
    /// conservative direction: a missed hard-bounce suppression costs
    /// one more failed send next time; an over-eager suppression on a
    /// merely-soft bounce silently opts a live subscriber out.
    #[serde(default)]
    pub(crate) hard: bool,
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Body")
            .field("send_id", &self.send_id)
            .field("email", &"<redacted>")
            .field("kind", &self.kind)
            .field("hard", &self.hard)
            .finish()
    }
}

/// Handle `POST /webhooks/delivery-events`.
///
/// # Errors
///
/// Returns [`Error::Unauthorized`] when the bearer token is missing or
/// wrong, [`Error::BadRequest`] on an oversized body or malformed JSON,
/// [`Error::NotFound`] when `(send_id, email)` names no delivery this
/// server recorded, [`Error::Store`] on a fjall failure.
pub(crate) async fn post(State(state): State<AppState>, request: Request) -> Result<StatusCode> {
    if !check_bearer(
        request.headers(),
        state.config.webhook_auth_token.expose_secret(),
    ) {
        return Err(Error::Unauthorized);
    }

    let body_bytes = axum::body::to_bytes(request.into_body(), WEBHOOK_BODY_LIMIT_BYTES)
        .await
        .map_err(|e| Error::BadRequest {
            reason: format!("read body: {e}"),
        })?;
    let body: Body = serde_json::from_slice(&body_bytes).map_err(|e| Error::BadRequest {
        reason: format!("parse JSON: {e}"),
    })?;

    let email_norm = body.email.trim().to_ascii_lowercase();
    let now = OffsetDateTime::now_utc();

    let Some(mut delivery) = state.store.delivery_get(&body.send_id, &email_norm)? else {
        return Err(Error::NotFound);
    };

    delivery.status = match body.kind {
        EventKind::Bounce => DeliveryStatus::Bounced,
        EventKind::Complaint => DeliveryStatus::Complained,
    };
    delivery.at = now;
    state.store.delivery_put(&delivery)?;

    let suppresses =
        body.kind == EventKind::Complaint || (body.kind == EventKind::Bounce && body.hard);
    if suppresses {
        suppress_subscriber(&state, &email_norm, now)?;
    }

    Ok(StatusCode::OK)
}

/// Flip an `Active` subscriber to `Unsubscribed`, bumping the consent
/// generation exactly like a visitor-driven unsubscribe
/// (`handlers/unsubscribe.rs::apply`). A separate call site from that
/// one on purpose — the two share the state-transition SHAPE (Active ->
/// Unsubscribed, generation+1) but not its trigger: this one runs on a
/// relay-authenticated system event, never a subscriber's own token.
///
/// Idempotent: a missing row or one already `Unsubscribed` is a no-op,
/// so replaying the same webhook event (a relay retry after a timeout)
/// never double-bumps the generation.
///
/// # Errors
///
/// Returns [`Error::Store`] on a fjall failure.
fn suppress_subscriber(state: &AppState, email: &str, now: OffsetDateTime) -> Result<()> {
    let Some(mut subscriber) = state.store.subscriber_get(email)? else {
        return Ok(());
    };
    if subscriber.state != SubscriberState::Active {
        return Ok(());
    }
    subscriber.state = SubscriberState::Unsubscribed;
    subscriber.unsubscribed_at = Some(now);
    subscriber.generation += 1;
    state.store.subscriber_put(&subscriber)
}
