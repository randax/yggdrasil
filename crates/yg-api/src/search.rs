//! The `search` Verb endpoint, kept as a thin transport over the engine.

use std::sync::Arc;

use crate::AppState;
use crate::error::ApiError;
use crate::wire::{Wire, WireJson};
use axum::extract::State;
use axum::response::{IntoResponse, Response};

/// `POST /v1/verbs/search` (RFC 0001 §7): lexical search over indexed repos.
pub(crate) async fn verb_search(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<yg_verbs::SearchRequest>,
) -> Result<Response, ApiError> {
    let response = state.search(req).await?;
    Ok(Wire(response).into_response())
}
