//! `GET /archive` - public archive of past sends. Phase 1: stub.
//! Phase 2 walks the `sends` partition and renders an index of past
//! issues + a per-send detail page.

use axum::{extract::State, response::IntoResponse};
use maud::html;

use crate::AppState;
use crate::error::Result;

/// Handle `GET /archive`.
///
/// # Errors
///
/// None in the stub. Phase 2 may surface [`crate::error::Error::Store`]
/// when iterating the partition fails.
pub(crate) async fn get(State(state): State<AppState>) -> Result<impl IntoResponse> {
    let brand = state.config.brand.name.clone();
    Ok(html! {
        h1 { "Archive" }
        p { "Past notes from " (brand) " will appear here once the first issue is sent." }
    })
}
