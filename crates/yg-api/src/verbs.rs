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
use yg_verbs::{
    RepoQualifier, ResolveError, ResolvedFts, ResolvedFuzzyShard, ResolvedShard, SearchTarget,
    ShardResolver, ShardRevision,
};

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
        match self
            .shards
            .leased_graph_path(target.repo_id, &revision)
            .await
        {
            Ok(leased) => Ok(ResolvedShard {
                path: leased.path,
                revision,
                cache_lease: Some(leased.lease),
            }),
            Err(error) => Err(map_cache_error(error)),
        }
    }

    async fn resolve_search_target(
        &self,
        qualifier: &RepoQualifier,
    ) -> Result<SearchTarget, ResolveError> {
        let target = self
            .control
            .verb_target(qualifier.as_str())
            .await
            .map_err(ResolveError::Internal)?
            .ok_or(ResolveError::UnknownRepo)?;
        let revision = target.revision.ok_or(ResolveError::NotIndexed)?;
        Ok(SearchTarget::new(
            target.repo_id,
            qualifier.clone(),
            ShardRevision::new(revision),
        ))
    }

    async fn indexed_search_targets(&self) -> Result<Vec<SearchTarget>, ResolveError> {
        self.control
            .indexed_repos(&yg_shard::syntactic_revision_suffix())
            .await
            .map_err(ResolveError::Internal)
            .map(|repos| {
                repos
                    .into_iter()
                    .map(|repo| {
                        SearchTarget::new(
                            repo.repo_id,
                            RepoQualifier::new(repo.qualifier),
                            ShardRevision::new(repo.revision),
                        )
                    })
                    .collect()
            })
    }

    async fn resolve_fts(&self, target: &SearchTarget) -> Result<ResolvedFts, ResolveError> {
        self.shards
            .leased_fts_path(target.repo_id(), target.revision().as_str())
            .await
            .map(|leased| ResolvedFts {
                path: leased.path,
                cache_lease: Some(leased.lease),
            })
            .map_err(map_cache_error)
    }

    async fn resolve_fuzzy(
        &self,
        qualifier: &RepoQualifier,
        pinned: Option<ShardRevision>,
    ) -> Result<ResolvedFuzzyShard, ResolveError> {
        let target = self
            .control
            .verb_target(qualifier.as_str())
            .await
            .map_err(ResolveError::Internal)?
            .ok_or(ResolveError::UnknownRepo)?;
        let Some(revision) = pinned
            .map(|revision| revision.as_str().to_string())
            .or(target.revision)
        else {
            return Err(ResolveError::NotIndexed);
        };
        let graph = self
            .shards
            .leased_graph_path(target.repo_id, &revision)
            .await
            .map_err(map_cache_error)?;
        let fts = self
            .shards
            .leased_fts_path_without_wait(target.repo_id, &revision)
            .await
            .map_err(map_cache_error)?;
        Ok(ResolvedFuzzyShard {
            graph_path: graph.path,
            fts_path: fts.path,
            revision: ShardRevision::new(revision),
            graph_cache_lease: Some(graph.lease),
            fts_cache_lease: Some(fts.lease),
        })
    }
}

fn map_cache_error(error: anyhow::Error) -> ResolveError {
    if error.downcast_ref::<yg_shard::RevisionMissing>().is_some() {
        ResolveError::RevisionMissing(error)
    } else if error.downcast_ref::<yg_shard::SchemaOutdated>().is_some() {
        ResolveError::SchemaOutdated
    } else {
        ResolveError::Internal(error)
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
