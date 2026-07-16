use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::fanout::RankedRepo;
use super::{RepoQualifier, SearchHit, SearchSnippet, SearchTargetProvenance};
use crate::engine::ShardResolver;

/// Drop retained handles for repositories absent from the selected page.
pub(super) fn retain_page_repos(
    mut ranked: Vec<RankedRepo>,
    page: &[SearchHit],
) -> Vec<RankedRepo> {
    let page_repos: HashSet<&RepoQualifier> = page.iter().map(|hit| &hit.repo).collect();
    ranked.retain(|repo| page_repos.contains(&repo.target.qualifier));
    ranked
}

/// Hydrate snippets by resolving a fresh leased handle for every selected repository.
pub(super) async fn hydrate_snippets<R: ShardResolver + 'static>(
    resolver: Arc<R>,
    ranked: Vec<RankedRepo>,
    target_provenance: SearchTargetProvenance,
    query: &str,
    page: &mut [SearchHit],
) {
    let mut by_repo: HashMap<RepoQualifier, Vec<usize>> = HashMap::new();
    for (position, hit) in page.iter().enumerate() {
        by_repo.entry(hit.repo.clone()).or_default().push(position);
    }

    for repo in ranked {
        let qualifier = repo.target.qualifier.clone();
        let Some(indices) = by_repo.remove(&qualifier) else {
            continue;
        };
        let resolved = match resolver.resolve_fts(&repo.target, target_provenance).await {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(
                    "fts path resolution failed during snippet hydration; returning hits unhighlighted: {error:#}"
                );
                continue;
            }
        };
        let path = resolved.path;
        let cache_lease = resolved.cache_lease;
        let (index, cache_lease) = match tokio::task::spawn_blocking(move || {
            let index = yg_shard::open_fts(&path)?;
            Ok::<_, anyhow::Error>((index, cache_lease))
        })
        .await
        {
            Ok(Ok(opened)) => opened,
            Ok(Err(error)) => {
                tracing::warn!(
                    "fts reopen failed during snippet hydration; returning hits unhighlighted: {error:#}"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!("fts reopen task panicked; returning hits unhighlighted: {error}");
                continue;
            }
        };
        hydrate_repo(index, cache_lease, query, &indices, page).await;
    }
}

async fn hydrate_repo(
    index: yg_shard::FtsIndex,
    cache_lease: Option<yg_shard::CacheLease>,
    query: &str,
    indices: &[usize],
    page: &mut [SearchHit],
) {
    let local_ids: Vec<String> = indices
        .iter()
        .map(|&position| {
            page[position]
                .id
                .local
                .clone()
                .expect("a search hit always has a segment-local id")
        })
        .collect();
    let query = query.to_string();
    let snippets = tokio::task::spawn_blocking(move || {
        let snippets = yg_shard::snippets_for(&index, &query, &local_ids);
        // Handle before lease: eviction is pin-gated, so the index must
        // close before its artifact becomes evictable.
        drop(index);
        drop(cache_lease);
        snippets
    })
    .await;
    let snippets = match snippets {
        Ok(Ok(snippets)) => snippets,
        Ok(Err(error)) => {
            tracing::warn!(
                "fts snippet generation failed; returning hits unhighlighted: {error:#}"
            );
            return;
        }
        Err(error) => {
            tracing::warn!("fts snippet task panicked; returning hits unhighlighted: {error}");
            return;
        }
    };
    for &position in indices {
        let local_id = page[position]
            .id
            .local
            .as_ref()
            .expect("a search hit always has a segment-local id");
        page[position].snippet = snippets.get(local_id).cloned().map(SearchSnippet::new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{ResolveError, ResolvedShard};
    use crate::search::{SearchTarget, ShardRevision};
    use crate::{SearchRequest, VerbId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ReopenResolver {
        path: std::path::PathBuf,
        target: SearchTarget,
        resolutions: AtomicUsize,
        control_revalidations: AtomicUsize,
    }

    impl ShardResolver for ReopenResolver {
        async fn resolve(
            &self,
            _qualifier: &str,
            _pinned: Option<crate::PinnedShard>,
        ) -> Result<ResolvedShard, ResolveError> {
            Err(ResolveError::UnknownRepo)
        }
        async fn resolve_search_target(
            &self,
            _qualifier: &RepoQualifier,
        ) -> Result<SearchTarget, ResolveError> {
            Ok(self.target.clone())
        }
        async fn indexed_search_targets(&self) -> Result<Vec<SearchTarget>, ResolveError> {
            Ok(vec![self.target.clone()])
        }
        async fn resolve_fts(
            &self,
            _target: &SearchTarget,
            provenance: SearchTargetProvenance,
        ) -> Result<crate::ResolvedFts, ResolveError> {
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            if provenance == SearchTargetProvenance::ResumedFromCursor {
                self.control_revalidations.fetch_add(1, Ordering::SeqCst);
            }
            Ok(crate::ResolvedFts {
                path: self.path.clone(),
                cache_lease: None,
            })
        }
    }

    fn fixture() -> (std::path::PathBuf, SearchTarget, SearchHit) {
        static TEMP_ID: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "yg-verbs-search-reopen-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let packed = yg_shard::build_fts(&[yg_shard::SearchDoc {
            node_id: "file:README.md".to_string(),
            kind: yg_shard::NodeKind::File,
            name: Some("README.md".to_string()),
            path: Some("README.md".to_string()),
            content: "this project applies a rate limit".to_string(),
        }])
        .expect("build fixture FTS");
        yg_shard::unpack_fts(&packed, &path).expect("unpack fixture FTS");
        let qualifier = RepoQualifier::new("repo".to_string());
        let target = SearchTarget::new(
            1,
            qualifier.clone(),
            ShardRevision::new("revision".to_string()),
        );
        let hit = SearchHit {
            id: VerbId::parse("file:repo:README.md").expect("id"),
            kind: yg_shard::NodeKind::File,
            name: None,
            path: None,
            repo: qualifier,
            score: 1.0,
            snippet: None,
        };
        (path, target, hit)
    }

    #[test]
    fn page_repo_without_retained_handle_reopens_and_hydrates() {
        let (path, target, hit) = fixture();
        let resolver = Arc::new(ReopenResolver {
            path: path.clone(),
            target: target.clone(),
            resolutions: AtomicUsize::new(0),
            control_revalidations: AtomicUsize::new(0),
        });
        let ranked = vec![RankedRepo {
            target,
            hits: vec![],
        }];
        let mut page = vec![hit];
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(hydrate_snippets(
                resolver.clone(),
                ranked,
                SearchTargetProvenance::FreshlyEnumerated,
                "rate limit",
                &mut page,
            ));
        assert!(page[0].snippet.is_some());
        assert_eq!(resolver.resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.control_revalidations.load(Ordering::SeqCst), 0);
        std::fs::remove_dir_all(path).expect("remove fixture");
    }

    #[test]
    fn search_reopens_the_ranked_page_with_a_fresh_resolution() {
        let (path, target, _) = fixture();
        let resolver = Arc::new(ReopenResolver {
            path: path.clone(),
            target,
            resolutions: AtomicUsize::new(0),
            control_revalidations: AtomicUsize::new(0),
        });

        let response = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(super::super::search(
                resolver.clone(),
                &crate::cursor::CursorCodec::new(
                    crate::CursorSecret::new(b"hydrate-test-secret-at-least-32-bytes".to_vec())
                        .expect("test secret is non-empty"),
                ),
                SearchRequest {
                    query: Some("rate limit".to_string()),
                    kinds: None,
                    repos: None,
                    mode: None,
                    limit: Some(1),
                    cursor: None,
                },
            ))
            .expect("search succeeds");

        assert_eq!(response.hits.len(), 1);
        assert!(response.hits[0].snippet.is_some());
        assert_eq!(
            resolver.resolutions.load(Ordering::SeqCst),
            2,
            "ranking and hydration each hold a lease for their complete index use"
        );
        assert_eq!(
            resolver.control_revalidations.load(Ordering::SeqCst),
            0,
            "a top-level fresh search must not revalidate either FTS resolution"
        );
        std::fs::remove_dir_all(path).expect("remove fixture");
    }
}
