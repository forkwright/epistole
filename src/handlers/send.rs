//! `POST /send` - operator-only. Auth via `Authorization: Bearer ...`
//! matching `config.send_auth_token`. Body is JSON `{subject, markdown}`.
//!
//! Phase 1: parse + auth + log a synthetic Send record (no SMTP yet).
//! Phase 2 wires `lettre` and walks the `Active` subscribers, recording
//! one [`crate::store::Delivery`] per recipient.

use axum::{
    extract::{Request, State},
    http::HeaderMap,
};
use pulldown_cmark::{Options, Parser, html};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;

use crate::AppState;
use crate::error::{Error, Result};
use crate::send_id::SendId;
use crate::store::Send;

/// Hard cap on `/send` request bodies. Mirrors the router-level
/// `RequestBodyLimitLayer` so the body collector here can't be tricked
/// by a stripped layer.
const SEND_BODY_LIMIT_BYTES: usize = 256 * 1024;

/// Request body for `POST /send`.
#[derive(Debug, Deserialize)]
pub(crate) struct Body {
    pub(crate) subject: String,
    pub(crate) markdown: String,
}

/// Reply payload for `POST /send`.
#[derive(Debug, Serialize)]
pub(crate) struct Reply {
    pub(crate) send_id: SendId,
    pub(crate) queued_recipients: usize,
}

/// Authenticate the bearer token in `Authorization: Bearer <token>`.
///
/// Hash-then-compare: SHA256 the presented token and the expected token,
/// compare the digests in constant time. This is more robust than the
/// branch-and-equalize approach because the final compare is always
/// against fixed-size 32-byte digests — no length-leak path, no
/// "uniform work on length mismatch" code that future refactors can
/// quietly drop.
fn check_bearer(headers: &HeaderMap, expected: &str) -> bool {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if presented.is_empty() {
        return false;
    }
    let presented_hash = Sha256::digest(presented.as_bytes());
    let expected_hash = Sha256::digest(expected.as_bytes());
    presented_hash.ct_eq(&expected_hash).into()
}

/// Handle `POST /send`. The body extractor is intentionally NOT in the
/// signature: we authenticate first, then read and parse the body —
/// otherwise a flood of unauthenticated 256 KiB POSTs would each cost
/// the service one full JSON parse before the bearer check fired.
///
/// # Errors
///
/// Returns [`Error::Unauthorized`] when the bearer token is missing or
/// wrong, [`Error::BadRequest`] on an oversized body, empty subject,
/// malformed JSON, or missing markdown, [`Error::Store`] on a fjall
/// failure.
pub(crate) async fn post(
    State(state): State<AppState>,
    request: Request,
) -> Result<axum::Json<Reply>> {
    // 1. Auth FIRST. Unauthenticated requests pay no body-collect or
    //    JSON-parse cost.
    if !check_bearer(
        request.headers(),
        state.config.send_auth_token.expose_secret(),
    ) {
        return Err(Error::Unauthorized);
    }

    // 2. Collect body with a hard cap. The router-level body limit also
    //    enforces this, but a defense-in-depth re-check inside the
    //    handler protects us if the layer is ever moved.
    let body_bytes = axum::body::to_bytes(request.into_body(), SEND_BODY_LIMIT_BYTES)
        .await
        .map_err(|e| Error::BadRequest {
            reason: format!("read body: {e}"),
        })?;

    // 3. Parse JSON.
    let body: Body = serde_json::from_slice(&body_bytes).map_err(|e| Error::BadRequest {
        reason: format!("parse JSON: {e}"),
    })?;

    if body.subject.trim().is_empty() {
        return Err(Error::BadRequest {
            reason: "subject is empty".to_owned(),
        });
    }
    if body.markdown.trim().is_empty() {
        return Err(Error::BadRequest {
            reason: "markdown body is empty".to_owned(),
        });
    }

    // 4. Render markdown to HTML, then sanitize. pulldown-cmark by
    //    default strips raw HTML blocks but it preserves URLs verbatim
    //    in markdown links (`[click](javascript:alert(1))` becomes
    //    `<a href="javascript:alert(1)">`). ammonia's default URL-scheme
    //    allowlist is conservative (http/https/mailto/etc.) — javascript:
    //    and data: are dropped.
    let mut html_raw = String::new();
    let parser = Parser::new_ext(
        &body.markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    );
    html::push_html(&mut html_raw, parser);
    let html_out = ammonia::clean(&html_raw);

    // 5. ULID send_id — lexicographic + monotonic + collision-resistant.
    //    Replaces wall-clock unix_timestamp_nanos which collided on
    //    same-nanosecond sends and was sensitive to clock step.
    let now = OffsetDateTime::now_utc();
    let send_id = SendId::generate();
    let send_rec = Send {
        id: send_id,
        subject: body.subject.clone(),
        body_html: html_out,
        sent_at: now,
    };
    // The reply hands `send_id` back to the operator, so the record is
    // durable before we return it — `send_put` fsyncs the journal.
    state.store.send_put(&send_rec)?;

    // Phase 2 (forkwright/epistole#1) walks the subscribers partition,
    // mints per-recipient unsubscribe tokens, sends via lettre, records
    // one Delivery per recipient. Phase 1 logs and returns 0 queued.
    // Log line is intentionally brief — no subject reflection.
    tracing::info!(send_id = %send_id, "send recorded (delivery pending phase 2)");

    Ok(axum::Json(Reply {
        send_id,
        queued_recipients: 0,
    }))
}
