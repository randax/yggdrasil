use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::fanout::RankedRepo;
use super::{RepoQualifier, SearchHit, SearchSnippet};
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

/// Hydrate snippets, reusing retained handles and reopening only handles that exceeded the cap.
pub(super) async fn hydrate_snippets<R: ShardResolver + 'static>(
    resolver: Arc<R>,
    ranked: Vec<RankedRepo>,
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
        let index = match repo.index {
            Some(index) => index,
            None => {
                let path = match resolver.resolve_fts(&repo.target).await {
                    Ok(path) => path,
                    Err(error) => {
                        tracing::warn!(
                            "fts path resolution failed during snippet hydration; returning hits unhighlighted: {error:#}"
                        );
                        continue;
                    }
                };
                match tokio::task::spawn_blocking(move || yg_shard::open_fts(&path)).await {
                    Ok(Ok(index)) => index,
                    Ok(Err(error)) => {
                        tracing::warn!(
                            "fts reopen failed during snippet hydration; returning hits unhighlighted: {error:#}"
                        );
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(
                            "fts reopen task panicked; returning hits unhighlighted: {error}"
                        );
                        continue;
                    }
                }
            }
        };
        hydrate_repo(index, query, &indices, page).await;
    }
}

async fn hydrate_repo(
    index: yg_shard::FtsIndex,
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
    let snippets =
        tokio::task::spawn_blocking(move || yg_shard::snippets_for(&index, &query, &local_ids))
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
    }

    impl ShardResolver for ReopenResolver {
        async fn resolve(
            &self,
            _qualifier: &str,
            _pinned: Option<String>,
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
        ) -> Result<std::path::PathBuf, ResolveError> {
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            Ok(self.path.clone())
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
        });
        let ranked = vec![RankedRepo {
            target,
            index: None,
            hits: vec![],
        }];
        let mut page = vec![hit];
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(hydrate_snippets(
                resolver.clone(),
                ranked,
                "rate limit",
                &mut page,
            ));
        assert!(page[0].snippet.is_some());
        assert_eq!(resolver.resolutions.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(path).expect("remove fixture");
    }

    #[test]
    fn retained_page_handle_is_reused_without_another_resolution() {
        let (path, target, hit) = fixture();
        let index = yg_shard::open_fts(&path).expect("open retained fixture handle");
        let resolver = Arc::new(ReopenResolver {
            path: path.clone(),
            target: target.clone(),
            resolutions: AtomicUsize::new(0),
        });
        let ranked = vec![RankedRepo {
            target,
            index: Some(index),
            hits: vec![],
        }];
        let mut page = vec![hit];
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(hydrate_snippets(
                resolver.clone(),
                ranked,
                "rate limit",
                &mut page,
            ));
        assert!(page[0].snippet.is_some());
        assert_eq!(
            resolver.resolutions.load(Ordering::SeqCst),
            0,
            "hydration reuses the retained ranking handle"
        );
        std::fs::remove_dir_all(path).expect("remove fixture");
    }

    #[test]
    fn search_reuses_the_ranked_page_handle_end_to_end() {
        let (path, target, _) = fixture();
        let resolver = Arc::new(ReopenResolver {
            path: path.clone(),
            target,
            resolutions: AtomicUsize::new(0),
        });

        let response = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(super::super::search(
                resolver.clone(),
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
            1,
            "ranking resolves once and hydration reuses that open index"
        );
        std::fs::remove_dir_all(path).expect("remove fixture");
    }
}
