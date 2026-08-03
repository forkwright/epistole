//! Public archive of past sends.

use axum::{
    extract::{Path, State},
    http::header,
    response::IntoResponse,
};

use crate::AppState;
use crate::error::{Error, Result};
use crate::send_id::SendId;
use crate::templates;

const INDEX_CACHE_CONTROL: &str = "public, max-age=300";
const DETAIL_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Most recent sends rendered on the archive index.
///
/// WHY: `GET /archive` is unauthenticated and the sends partition only
/// ever grows, so rendering every record lets accumulated history decide
/// both the response size and the memory one request costs. Capping the
/// page keeps that bounded; the newest sends are the ones a reader wants
/// and every send stays reachable at its own permanent detail URL.
const INDEX_LIMIT: usize = 100;

/// Handle `GET /archive`.
///
/// # Errors
///
/// Returns [`Error::Store`] when reading the sends partition fails.
pub(crate) async fn get(State(state): State<AppState>) -> Result<impl IntoResponse> {
    // WHY: ask for one past the cap so a full page can be distinguished
    // from a truncated one without counting the whole partition.
    let mut sends = state.store.recent_sends(INDEX_LIMIT + 1)?;
    let truncated = sends.len() > INDEX_LIMIT;
    sends.truncate(INDEX_LIMIT);

    Ok((
        [(header::CACHE_CONTROL, INDEX_CACHE_CONTROL)],
        templates::archive_index(&state.config.brand.name, &sends, truncated),
    ))
}

/// Handle `GET /archive/{send_id}`.
///
/// # Errors
///
/// Returns [`Error::Store`] when reading the send fails, or
/// [`Error::NotFound`] when the send id does not exist.
pub(crate) async fn detail(
    State(state): State<AppState>,
    Path(send_id): Path<SendId>,
) -> Result<impl IntoResponse> {
    let send = state.store.send_get(&send_id)?.ok_or(Error::NotFound)?;

    Ok((
        [(header::CACHE_CONTROL, DETAIL_CACHE_CONTROL)],
        templates::archive_detail(&state.config.brand.name, &send),
    ))
}
