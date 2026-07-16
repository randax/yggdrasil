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
use yg_control::{ControlPlane, ShardState};
use yg_verbs::{
    PinnedShard, RepoQualifier, ResolveError, ResolvedFts, ResolvedFuzzyShard, ResolvedShard,
    SearchTarget, ShardResolver, ShardRevision,
};

use crate::AppState;
use crate::error::ApiError;
use crate::wire::{Wire, WireJson};

/// How this deployment reaches Shards: repo qualifiers resolve through
/// the control plane (re-resolved on every call, so a pointer swap is
/// picked up by the next query without a restart; `pinned` — from a
/// pagination cursor — bypasses the pointer and reads that exact
/// immutable revision), and segments come off the local cache. Pinned
/// revisions are admitted only while their repo remains visible and
/// their control-plane row remains published.
pub(crate) struct ShardAccess {
    control: ControlPlane,
    shards: Arc<yg_shard::ShardCache>,
}

impl ShardAccess {
    pub(crate) fn new(control: ControlPlane, shards: Arc<yg_shard::ShardCache>) -> Self {
        Self { control, shards }
    }

    async fn require_published_revision(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> Result<(), ResolveError> {
        let state = self
            .control
            .shard_state(repo_id, revision)
            .await
            .map_err(ResolveError::Internal)?;
        published_revision(state, revision)
    }
}

impl ShardResolver for ShardAccess {
    async fn resolve(
        &self,
        qualifier: &str,
        pinned: Option<PinnedShard>,
    ) -> Result<ResolvedShard, ResolveError> {
        let target = self
            .control
            .verb_target(qualifier)
            .await
            .map_err(ResolveError::Internal)?;
        let target = match target {
            Some(target) => target,
            None => {
                return Err(missing_visible_target(
                    pinned.as_ref().map(|pinned| pinned.revision.as_str()),
                ));
            }
        };
        let revision = match pinned {
            Some(pinned) => {
                matching_repo_id(pinned.repo_id, target.repo_id, pinned.revision.as_str())?;
                self.require_published_revision(target.repo_id, pinned.revision.as_str())
                    .await?;
                pinned.revision
            }
            None => match target.revision {
                Some(revision) => ShardRevision::new(revision),
                None => return Err(ResolveError::NotIndexed),
            },
        };
        match self
            .shards
            .leased_graph_path(target.repo_id, revision.as_str())
            .await
        {
            Ok(leased) => Ok(ResolvedShard {
                path: leased.path,
                repo_id: target.repo_id,
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
        let visible = self
            .control
            .verb_target(target.qualifier().as_str())
            .await
            .map_err(ResolveError::Internal)?
            .ok_or_else(|| revision_missing(target.revision().as_str()))?;
        matching_repo_id(
            target.repo_id(),
            visible.repo_id,
            target.revision().as_str(),
        )?;
        self.require_published_revision(target.repo_id(), target.revision().as_str())
            .await?;
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
        pinned: Option<PinnedShard>,
    ) -> Result<ResolvedFuzzyShard, ResolveError> {
        let target = self
            .control
            .verb_target(qualifier.as_str())
            .await
            .map_err(ResolveError::Internal)?;
        let target = match target {
            Some(target) => target,
            None => {
                return Err(missing_visible_target(
                    pinned.as_ref().map(|pinned| pinned.revision.as_str()),
                ));
            }
        };
        let revision = match pinned {
            Some(pinned) => {
                matching_repo_id(pinned.repo_id, target.repo_id, pinned.revision.as_str())?;
                self.require_published_revision(target.repo_id, pinned.revision.as_str())
                    .await?;
                pinned.revision
            }
            None => match target.revision {
                Some(revision) => ShardRevision::new(revision),
                None => return Err(ResolveError::NotIndexed),
            },
        };
        let fts = self
            .shards
            .leased_fts_path(target.repo_id, revision.as_str())
            .await
            .map_err(map_cache_error)?;
        Ok(ResolvedFuzzyShard {
            repo_id: target.repo_id,
            fts_path: fts.path,
            revision,
            fts_cache_lease: Some(fts.lease),
        })
    }
}

fn missing_visible_target(pinned_revision: Option<&str>) -> ResolveError {
    pinned_revision.map_or(ResolveError::UnknownRepo, revision_missing)
}

fn matching_repo_id(expected: i64, visible: i64, revision: &str) -> Result<(), ResolveError> {
    if expected == visible {
        Ok(())
    } else {
        Err(revision_missing(revision))
    }
}

fn published_revision(state: Option<ShardState>, revision: &str) -> Result<(), ResolveError> {
    match state {
        Some(ShardState::Published) => Ok(()),
        Some(ShardState::Reclaiming) | None => Err(revision_missing(revision)),
    }
}

fn revision_missing(revision: &str) -> ResolveError {
    ResolveError::RevisionMissing(anyhow::Error::new(yg_shard::RevisionMissing {
        revision: revision.to_string(),
    }))
}

fn map_cache_error(error: anyhow::Error) -> ResolveError {
    if error.downcast_ref::<yg_shard::RevisionMissing>().is_some() {
        ResolveError::RevisionMissing(error)
    } else if error.downcast_ref::<yg_shard::SchemaOutdated>().is_some() {
        ResolveError::SchemaOutdated
    } else if error
        .downcast_ref::<yg_shard::CacheCapacityUnavailable>()
        .is_some()
        || error
            .downcast_ref::<yg_shard::CacheArtifactTooLarge>()
            .is_some()
    {
        ResolveError::CacheCapacityUnavailable
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use object_store::ObjectStoreExt;

    const TEST_REPO_ID: i64 = 45;
    const TEST_QUALIFIER: &str = "github.com/acme/widgets";

    struct SequencingResolver {
        cache: Arc<yg_shard::ShardCache>,
        revision: String,
        graph_calls: Arc<AtomicUsize>,
        graph_revision: Arc<std::sync::Mutex<Option<String>>>,
    }

    impl ShardResolver for SequencingResolver {
        async fn resolve(
            &self,
            qualifier: &str,
            pinned: Option<PinnedShard>,
        ) -> Result<ResolvedShard, ResolveError> {
            assert_eq!(qualifier, TEST_QUALIFIER);
            self.graph_calls.fetch_add(1, Ordering::SeqCst);
            let pinned = pinned.ok_or(ResolveError::NotIndexed)?;
            assert_eq!(pinned.repo_id, TEST_REPO_ID);
            let revision = pinned.revision;
            *self.graph_revision.lock().expect("revision lock poisoned") =
                Some(revision.as_str().to_string());
            let leased = self
                .cache
                .leased_graph_path(TEST_REPO_ID, revision.as_str())
                .await
                .map_err(ResolveError::Internal)?;
            Ok(ResolvedShard {
                path: leased.path,
                repo_id: TEST_REPO_ID,
                revision,
                cache_lease: Some(leased.lease),
            })
        }

        async fn resolve_search_target(
            &self,
            _qualifier: &RepoQualifier,
        ) -> Result<SearchTarget, ResolveError> {
            Err(ResolveError::Internal(anyhow::anyhow!(
                "unused search-target resolver"
            )))
        }

        async fn indexed_search_targets(&self) -> Result<Vec<SearchTarget>, ResolveError> {
            Err(ResolveError::Internal(anyhow::anyhow!(
                "unused indexed-target resolver"
            )))
        }

        async fn resolve_fts(&self, _target: &SearchTarget) -> Result<ResolvedFts, ResolveError> {
            Err(ResolveError::Internal(anyhow::anyhow!(
                "unused search FTS resolver"
            )))
        }

        async fn resolve_fuzzy(
            &self,
            qualifier: &RepoQualifier,
            pinned: Option<PinnedShard>,
        ) -> Result<ResolvedFuzzyShard, ResolveError> {
            assert_eq!(qualifier.as_str(), TEST_QUALIFIER);
            let revision = pinned
                .map(|pinned| {
                    assert_eq!(pinned.repo_id, TEST_REPO_ID);
                    pinned.revision
                })
                .unwrap_or_else(|| ShardRevision::new(self.revision.clone()));
            let leased = self
                .cache
                .leased_fts_path(TEST_REPO_ID, revision.as_str())
                .await
                .map_err(ResolveError::Internal)?;
            Ok(ResolvedFuzzyShard {
                repo_id: TEST_REPO_ID,
                fts_path: leased.path,
                revision,
                fts_cache_lease: Some(leased.lease),
            })
        }
    }

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "yg-api-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create test cache directory");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn regular_file_bytes(root: &std::path::Path) -> u64 {
        let mut pending = vec![root.to_path_buf()];
        let mut bytes = 0;
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory)
                .expect("read test cache directory")
                .flatten()
            {
                let metadata = entry.metadata().expect("read cached artifact metadata");
                if metadata.is_dir() {
                    pending.push(entry.path());
                } else if metadata.is_file() {
                    bytes += metadata.len();
                }
            }
        }
        bytes
    }

    async fn fuzzy_fixture_result(
        names: &[&str],
        query: &str,
    ) -> (
        Result<yg_verbs::AddressedResponse<yg_verbs::NodeResponse>, yg_verbs::VerbError>,
        usize,
    ) {
        let store = Arc::new(object_store::memory::InMemory::new());
        let nodes = names
            .iter()
            .enumerate()
            .map(|(index, name)| yg_shard::Node::symbol(&format!("src/{index}.rs"), name, 1))
            .collect::<Vec<_>>();
        let search_docs = nodes
            .iter()
            .map(|node| yg_shard::SearchDoc {
                node_id: node.id.clone(),
                kind: yg_shard::NodeKind::Symbol,
                name: node.name.clone(),
                path: node.path.clone(),
                content: String::new(),
            })
            .collect();
        let published = yg_shard::write_shard(
            store.as_ref(),
            TEST_REPO_ID,
            query,
            yg_shard::Graph {
                nodes,
                edges: Vec::new(),
            },
            search_docs,
        )
        .await
        .expect("publish fuzzy fixture");
        let directory = TestDirectory::new("fuzzy-cardinality");
        let graph_calls = Arc::new(AtomicUsize::new(0));
        let engine = yg_verbs::Engine::new(
            SequencingResolver {
                cache: Arc::new(yg_shard::ShardCache::new(store, directory.path())),
                revision: published.revision,
                graph_calls: graph_calls.clone(),
                graph_revision: Arc::new(std::sync::Mutex::new(None)),
            },
            yg_verbs::CursorSecret::new(b"api-verbs-test-secret-at-least-32-bytes".to_vec())
                .expect("test secret"),
        );
        let result = engine
            .node(yg_verbs::NodeRequest {
                id: query.to_string(),
                repo: Some(RepoQualifier::new(TEST_QUALIFIER.to_string())),
                path: None,
            })
            .await;
        (result, graph_calls.load(Ordering::SeqCst))
    }

    #[test]
    fn oversized_cache_artifacts_are_typed_unavailable_errors() {
        let error = anyhow::Error::new(yg_shard::CacheArtifactTooLarge {
            artifact_bytes: 2,
            capacity: yg_shard::CacheCapacity::new(1).expect("non-zero capacity"),
        });

        assert!(matches!(
            map_cache_error(error),
            ResolveError::CacheCapacityUnavailable
        ));
    }

    #[test]
    fn only_published_pinned_revisions_are_admitted() {
        assert!(published_revision(Some(ShardState::Published), "rev").is_ok());

        for state in [None, Some(ShardState::Reclaiming)] {
            let error = published_revision(state, "rev").expect_err("revision is unavailable");
            let ResolveError::RevisionMissing(source) = error else {
                panic!("unavailable revisions must use the cursor-expiry category");
            };
            assert_eq!(
                source
                    .downcast_ref::<yg_shard::RevisionMissing>()
                    .expect("typed Shard missing source")
                    .revision,
                "rev"
            );
        }
    }

    #[test]
    fn search_target_repo_mismatch_is_a_missing_revision() {
        let error = matching_repo_id(45, 46, "rev").expect_err("repo id must match qualifier");
        assert!(matches!(error, ResolveError::RevisionMissing(_)));
        assert!(matching_repo_id(45, 45, "rev").is_ok());
    }

    #[test]
    fn hidden_pinned_repo_is_a_missing_revision() {
        assert!(matches!(
            missing_visible_target(Some("rev")),
            ResolveError::RevisionMissing(_)
        ));
        assert!(matches!(
            missing_visible_target(None),
            ResolveError::UnknownRepo
        ));
    }

    #[tokio::test]
    async fn unique_fuzzy_resolution_releases_fts_before_loading_pinned_graph() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let graph_node = yg_shard::Node::symbol("src/lib.rs", "Unique", 1);
        let published = yg_shard::write_shard(
            store.as_ref(),
            TEST_REPO_ID,
            "unique-fuzzy",
            yg_shard::Graph {
                nodes: vec![graph_node.clone()],
                edges: Vec::new(),
            },
            vec![yg_shard::SearchDoc {
                node_id: graph_node.id,
                kind: yg_shard::NodeKind::Symbol,
                name: Some("Unique".to_string()),
                path: Some("src/lib.rs".to_string()),
                content: String::new(),
            }],
        )
        .await
        .expect("publish fuzzy fixture");

        let measurement_directory = TestDirectory::new("fuzzy-measurement");
        let measurement_cache =
            yg_shard::ShardCache::new(store.clone(), measurement_directory.path());
        measurement_cache
            .fts_path(TEST_REPO_ID, &published.revision)
            .await
            .expect("measure unpacked FTS bundle");
        let fts_bytes = regular_file_bytes(measurement_directory.path());
        let graph_bytes = store
            .head(&yg_shard::graph_segment_key(TEST_REPO_ID, &published.revision).into())
            .await
            .expect("published graph metadata")
            .size;
        let capacity_bytes = graph_bytes.max(fts_bytes);
        assert!(
            capacity_bytes < graph_bytes + fts_bytes,
            "the cap fits either artifact but not both"
        );

        let bounded_directory = TestDirectory::new("fuzzy-bounded");
        let cache = Arc::new(yg_shard::ShardCache::with_capacity(
            store,
            bounded_directory.path(),
            yg_shard::CacheCapacity::new(capacity_bytes).expect("non-zero cache capacity"),
        ));
        let graph_calls = Arc::new(AtomicUsize::new(0));
        let graph_revision = Arc::new(std::sync::Mutex::new(None));
        let engine = yg_verbs::Engine::new(
            SequencingResolver {
                cache,
                revision: published.revision.clone(),
                graph_calls: graph_calls.clone(),
                graph_revision: graph_revision.clone(),
            },
            yg_verbs::CursorSecret::new(b"api-verbs-test-secret-at-least-32-bytes".to_vec())
                .expect("test secret"),
        );

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.node(yg_verbs::NodeRequest {
                id: "Unique".to_string(),
                repo: Some(RepoQualifier::new(TEST_QUALIFIER.to_string())),
                path: None,
            }),
        )
        .await
        .expect("graph acquisition must not wait on the FTS lease")
        .expect("unique fuzzy node resolves");

        assert!(matches!(response, yg_verbs::AddressedResponse::Resolved(_)));
        assert_eq!(graph_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            graph_revision
                .lock()
                .expect("revision lock poisoned")
                .as_deref(),
            Some(published.revision.as_str()),
            "the graph loads the exact revision used by the FTS lookup"
        );
    }

    #[tokio::test]
    async fn non_unique_fuzzy_resolution_never_loads_the_graph() {
        let (missing, missing_graph_calls) = fuzzy_fixture_result(&["Different"], "Missing").await;
        assert!(matches!(missing, Err(yg_verbs::VerbError::NoSuchSymbol(_))));
        assert_eq!(missing_graph_calls, 0);

        let (ambiguous, ambiguous_graph_calls) =
            fuzzy_fixture_result(&["Shared", "Shared"], "Shared").await;
        assert!(matches!(
            ambiguous,
            Ok(yg_verbs::AddressedResponse::Ambiguous(_))
        ));
        assert_eq!(ambiguous_graph_calls, 0);
    }
}
