//! The node-addressed Verb endpoints, as thin encoders: each handler
//! hands the request to the engine and puts the typed result (or the
//! sanitized error) on the wire. The Verb contract itself — cursors,
//! validation, Shard resolution, blocking execution — lives in
//! `yg_verbs::engine`; this module's one piece of substance is the
//! [`ShardAccess`] resolver wiring the engine to the control plane and
//! the segment cache.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use yg_control::ControlPlane;
use yg_verbs::{ResolveError, ResolvedShard, ShardResolver};

use crate::AppState;
use crate::error::ApiError;
use crate::wire::{Wire, WireJson};

/// How this deployment reaches Shards: repo qualifiers resolve through
/// the control plane (re-resolved on every call, so a pointer swap is
/// picked up by the next query without a restart; `pinned` — from a
/// pagination cursor — bypasses the pointer and reads that exact
/// immutable revision), and segments come off the local cache.
pub(crate) struct ShardAccess {
    control: ControlPlane,
    shards: Arc<yg_shard::ShardCache>,
}

impl ShardAccess {
    pub(crate) fn new(control: ControlPlane, shards: Arc<yg_shard::ShardCache>) -> Self {
        Self { control, shards }
    }
}

impl ShardResolver for ShardAccess {
    async fn resolve(
        &self,
        qualifier: &str,
        pinned: Option<String>,
    ) -> Result<ResolvedShard, ResolveError> {
        let target = self
            .control
            .verb_target(qualifier)
            .await
            .map_err(ResolveError::Internal)?
            .ok_or(ResolveError::UnknownRepo)?;
        let Some(revision) = pinned.or(target.revision) else {
            return Err(ResolveError::NotIndexed);
        };
        match self.shards.graph_path(target.repo_id, &revision).await {
            Ok(path) => Ok(ResolvedShard { path, revision }),
            Err(e) if e.downcast_ref::<yg_shard::RevisionMissing>().is_some() => {
                Err(ResolveError::RevisionMissing(e))
            }
            Err(e) if e.downcast_ref::<yg_shard::SchemaOutdated>().is_some() => {
                Err(ResolveError::SchemaOutdated)
            }
            Err(e) => Err(ResolveError::Internal(e)),
        }
    }
}

/// `POST /v1/verbs/node` (RFC 0001 §7): full node + edge summary.
pub(crate) async fn verb_node(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<yg_verbs::NodeRequest>,
) -> Result<Response, ApiError> {
    let response = state.engine.node(req).await?;
    Ok(Wire(response).into_response())
}

/// `POST /v1/verbs/neighbors` (RFC 0001 §7): one page of the adjacent
/// subgraph.
pub(crate) async fn verb_neighbors(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<yg_verbs::NeighborsRequest>,
) -> Result<Response, ApiError> {
    let response = state.engine.neighbors(req).await?;
    Ok(Wire(response).into_response())
}

/// `POST /v1/verbs/history` (RFC 0001 §7): commits touching a File (or a
/// Symbol's defining file), newest-first, with a `since` floor and cursor
/// pagination.
pub(crate) async fn verb_history(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<yg_verbs::HistoryRequest>,
) -> Result<Response, ApiError> {
    let response = state.engine.history(req).await?;
    Ok(Wire(response).into_response())
}
