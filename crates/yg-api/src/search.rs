//! The `search` Verb endpoint, kept as a thin transport over the engine.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use yg_verbs::SearchWireResponse;

use crate::AppState;
use crate::error::ApiError;
use crate::wire::{Wire, WireJson};

/// `POST /v1/verbs/search` (RFC 0001 §7): lexical search over indexed repos.
pub(crate) async fn verb_search(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<yg_verbs::SearchRequest>,
) -> Result<Response, ApiError> {
    let response = state.search(req).await?;
    Ok(Wire(SearchWireResponse::from(response)).into_response())
}
