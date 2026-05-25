//! Public archive of past sends.

use axum::{
    extract::{Path, State},
    http::header,
    response::IntoResponse,
};

use crate::AppState;
use crate::error::{Error, Result};
use crate::templates;

const INDEX_CACHE_CONTROL: &str = "public, max-age=300";
const DETAIL_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Handle `GET /archive`.
///
/// # Errors
///
/// Returns [`Error::Store`] when iterating the sends partition fails.
pub(crate) async fn get(State(state): State<AppState>) -> Result<impl IntoResponse> {
    let mut sends = state.store.iter_sends()?.collect::<Result<Vec<_>>>()?;
    sends.sort_by(|a, b| b.id.cmp(&a.id));

    Ok((
        [(header::CACHE_CONTROL, INDEX_CACHE_CONTROL)],
        templates::archive_index(&state.config.brand.name, &sends),
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
    Path(send_id): Path<String>,
) -> Result<impl IntoResponse> {
    let send = state.store.send_get(&send_id)?.ok_or(Error::NotFound)?;

    Ok((
        [(header::CACHE_CONTROL, DETAIL_CACHE_CONTROL)],
        templates::archive_detail(&state.config.brand.name, &send),
    ))
}
