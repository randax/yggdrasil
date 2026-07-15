//! The `search` Verb endpoint, kept as a thin transport over the engine.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::AppState;
use crate::error::ApiError;
use crate::wire::{Wire, WireJson};

#[derive(Serialize)]
struct SearchWireResponse {
    hits: Vec<yg_verbs::SearchHit>,
    next_cursor: Option<String>,
}

/// `POST /v1/verbs/search` (RFC 0001 §7): lexical search over indexed repos.
pub(crate) async fn verb_search(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<yg_verbs::SearchRequest>,
) -> Result<Response, ApiError> {
    let response = state.engine.search(req).await?;
    let next_cursor = response.next.as_ref().map(yg_verbs::cursor::encode);
    Ok(Wire(SearchWireResponse {
        hits: response.hits,
        next_cursor,
    })
    .into_response())
}
