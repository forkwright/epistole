//! `POST /send` - operator-only. Auth via `Authorization: Bearer ...`
//! matching `config.send_auth_token`. Body is JSON `{subject, markdown}`.
//!
//! Phase 1: parse + auth + log a synthetic Send record (no SMTP yet).
//! Phase 2 wires `lettre` and walks the `Active` subscribers, recording
//! one [`crate::store::Delivery`] per recipient.

use axum::{Json, extract::State, http::HeaderMap};
use pulldown_cmark::{Options, Parser, html};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::AppState;
use crate::error::{Error, Result};
use crate::store::Send;

/// Request body for `POST /send`.
#[derive(Debug, Deserialize)]
pub(crate) struct Body {
    pub(crate) subject: String,
    pub(crate) markdown: String,
}

/// Reply payload for `POST /send`.
#[derive(Debug, Serialize)]
pub(crate) struct Reply {
    pub(crate) send_id: String,
    pub(crate) queued_recipients: usize,
}

/// Handle `POST /send`.
///
/// # Errors
///
/// Returns [`Error::Unauthorized`] when the bearer token is missing or
/// wrong, [`Error::BadRequest`] on an empty subject or body,
/// [`Error::Store`] on a fjall failure.
pub(crate) async fn post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Body>,
) -> Result<Json<Reply>> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected = state.config.send_auth_token.expose_secret();
    if presented.is_empty() || presented != expected {
        return Err(Error::Unauthorized);
    }

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

    let mut html_out = String::new();
    let parser = Parser::new_ext(
        &body.markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    );
    html::push_html(&mut html_out, parser);

    let now = OffsetDateTime::now_utc();
    let send_id = now.unix_timestamp_nanos().to_string();
    let send_rec = Send {
        id: send_id.clone(),
        subject: body.subject.clone(),
        body_html: html_out,
        sent_at: now,
    };
    let bytes = serde_json::to_vec(&send_rec).map_err(|e| Error::Store {
        reason: format!("encode send: {e}"),
    })?;
    state
        .store
        .sends
        .insert(send_id.as_bytes(), bytes)
        .map_err(|e| Error::Store {
            reason: format!("sends partition write: {e}"),
        })?;

    // TODO(forkwright/epistole#1): walk the subscribers partition for
    // state == Active, mint per-recipient unsubscribe tokens, send via
    // lettre, record one Delivery per recipient. For now we log and
    // return a queued count of 0.
    tracing::info!(send_id = %send_id, subject = %body.subject, "send recorded (delivery pending phase 2)");

    Ok(Json(Reply {
        send_id,
        queued_recipients: 0,
    }))
}
