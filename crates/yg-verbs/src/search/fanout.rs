use std::collections::HashSet;
use std::sync::Arc;

use super::{RepoQualifier, SearchHit, SearchTarget};
use crate::engine::{ResolveError, ShardResolver, VerbError};

pub(super) struct RankedRepo {
    pub(super) target: SearchTarget,
    pub(super) index: Option<yg_shard::FtsIndex>,
    pub(super) hits: Vec<SearchHit>,
}

/// Retain at most the configured number of completed handles.
fn retain_completed_handle<I>(retained: &mut usize, index: I) -> Option<I> {
    if *retained < super::MAX_RETAINED_FTS_HANDLES {
        *retained += 1;
        Some(index)
    } else {
        None
    }
}

/// Resolve, open, and rank repositories with bounded concurrency and handle retention.
pub(super) async fn rank_targets<R: ShardResolver + 'static>(
    resolver: Arc<R>,
    targets: Vec<SearchTarget>,
    query: String,
    kinds: Option<Vec<yg_shard::NodeKind>>,
    from_cursor: bool,
) -> Result<Vec<RankedRepo>, VerbError> {
    let total = targets.len();
    let mut targets = targets.into_iter().enumerate();
    let mut ranked = Vec::with_capacity(total);
    let mut retained = 0;

    loop {
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..super::MAX_CONCURRENT_SEARCH_FANOUT {
            let Some((position, target)) = targets.next() else {
                break;
            };
            spawn_rank_task(
                &mut tasks,
                position,
                resolver.clone(),
                target,
                query.clone(),
                kinds.clone(),
                from_cursor,
            );
        }
        if tasks.is_empty() {
            break;
        }

        let mut batch = Vec::with_capacity(tasks.len());
        while let Some(joined) = tasks.join_next().await {
            let (position, result) = joined.map_err(|error| {
                VerbError::Internal(anyhow::Error::new(error).context("search task panicked"))
            })?;
            let result = result.map(|mut ranked| {
                let index = ranked
                    .index
                    .take()
                    .expect("ranking always returns an open index");
                ranked.index = retain_completed_handle(&mut retained, index);
                ranked
            });
            batch.push((position, result));
        }
        batch.sort_unstable_by_key(|(position, _)| *position);
        for (_, result) in batch {
            ranked.push(result?);
        }
    }
    Ok(ranked)
}

fn spawn_rank_task<R: ShardResolver + 'static>(
    tasks: &mut tokio::task::JoinSet<(usize, Result<RankedRepo, VerbError>)>,
    position: usize,
    resolver: Arc<R>,
    target: SearchTarget,
    query: String,
    kinds: Option<Vec<yg_shard::NodeKind>>,
    from_cursor: bool,
) {
    tasks.spawn(async move {
        let path = match resolver.resolve_fts(&target).await {
            Ok(path) => path,
            Err(error) => return (position, Err(map_search_shard_error(error, from_cursor))),
        };
        let qualifier = target.qualifier.clone();
        let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<RankedRepo> {
            let index = yg_shard::open_fts(&path)?;
            let local = yg_shard::search(
                &index,
                &yg_shard::SearchParams {
                    query: &query,
                    kinds: kinds.as_deref(),
                    limit: super::MAX_SEARCH_WINDOW,
                },
            )?;
            let hits = local
                .into_iter()
                .map(|hit| super::types::qualify_hit(qualifier.as_str(), hit))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(RankedRepo {
                target,
                index: Some(index),
                hits,
            })
        })
        .await;
        let result = match outcome {
            Ok(Ok(ranked)) => Ok(ranked),
            Ok(Err(error)) => Err(map_search_query_error(error)),
            Err(error) => Err(VerbError::Internal(
                anyhow::Error::new(error).context("search task panicked"),
            )),
        };
        (position, result)
    });
}

pub(super) fn parse_search_kinds(
    kinds: Option<&[String]>,
) -> Result<Option<Vec<yg_shard::NodeKind>>, String> {
    let Some(kinds) = kinds else { return Ok(None) };
    if kinds.is_empty() {
        return Err(
            "kinds must name at least one node kind; omit it to search every kind".to_string(),
        );
    }
    kinds
        .iter()
        .map(|kind| {
            yg_shard::NodeKind::parse(kind).ok_or_else(|| {
                let vocab: Vec<&str> = yg_shard::NodeKind::ALL.iter().map(|k| k.as_str()).collect();
                format!(
                    "unknown node kind {kind:?}: expected any of {}",
                    vocab.join(", ")
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

pub(super) async fn resolve_search_targets(
    resolver: &impl ShardResolver,
    repos: Option<&[String]>,
) -> Result<Vec<SearchTarget>, VerbError> {
    let targets = match repos {
        Some(repos) => {
            if repos.is_empty() {
                return Err(VerbError::BadRequest(
                    "repos must name at least one repository; omit it to search every indexed repo"
                        .to_string(),
                ));
            }
            let mut targets = Vec::with_capacity(repos.len());
            let mut seen = HashSet::new();
            for qualifier in repos {
                if !seen.insert(qualifier.as_str()) {
                    continue;
                }
                let qualifier = RepoQualifier::new(qualifier.clone());
                let target = resolver
                    .resolve_search_target(&qualifier)
                    .await
                    .map_err(|error| map_named_target_error(qualifier.as_str(), error))?;
                targets.push(target);
            }
            targets
        }
        None => resolver.indexed_search_targets().await.map_err(|error| {
            VerbError::Internal(
                anyhow::Error::new(error).context("listing indexed repositories for search"),
            )
        })?,
    };
    if targets.len() > super::MAX_SEARCH_TARGETS {
        return Err(VerbError::BadRequest(format!(
            "the search spans {} repositories, more than the {} one query covers; narrow it with the repos filter",
            targets.len(),
            super::MAX_SEARCH_TARGETS
        )));
    }
    Ok(targets)
}

pub(super) fn dedup_targets(targets: Vec<SearchTarget>) -> Vec<SearchTarget> {
    let mut seen = HashSet::new();
    targets
        .into_iter()
        .filter(|target| seen.insert(target.qualifier.clone()))
        .collect()
}

fn map_named_target_error(qualifier: &str, error: ResolveError) -> VerbError {
    match error {
        ResolveError::UnknownRepo => VerbError::NotFound(format!("no indexed repository matches {qualifier:?}")),
        ResolveError::NotIndexed => VerbError::NotFound(format!("{qualifier} is registered but not yet indexed; try again shortly")),
        ResolveError::RevisionMissing(source) | ResolveError::Internal(source) => VerbError::Internal(source),
        ResolveError::SchemaOutdated => VerbError::Unavailable(
            "a repo's Shard predates the current index schema and is being re-indexed; try again shortly".to_string()),
    }
}

pub(super) fn map_search_shard_error(error: ResolveError, from_cursor: bool) -> VerbError {
    match error {
        ResolveError::RevisionMissing(_) if from_cursor => VerbError::Gone(
            "this cursor's Shard revision is no longer available; restart the search without a cursor".to_string()),
        ResolveError::SchemaOutdated if from_cursor => VerbError::Gone(
            "this cursor's Shard revision predates the current index schema; restart the search without a cursor".to_string()),
        ResolveError::SchemaOutdated => VerbError::Unavailable(
            "a repo's Shard predates the current index schema and is being re-indexed; try again shortly".to_string()),
        ResolveError::RevisionMissing(source) | ResolveError::Internal(source) => VerbError::Internal(source),
        ResolveError::UnknownRepo => VerbError::NotFound("no indexed repository matches the search target".to_string()),
        ResolveError::NotIndexed => VerbError::NotFound(
            "a search target is registered but not yet indexed; try again shortly".to_string()),
    }
}

fn map_search_query_error(error: anyhow::Error) -> VerbError {
    match error.downcast_ref::<yg_shard::QueryMalformed>() {
        Some(malformed) => VerbError::BadRequest(malformed.to_string()),
        None => VerbError::Internal(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::ShardRevision;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropSpy(Arc<AtomicUsize>);
    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn handles_beyond_retention_bound_drop_at_ranking_completion() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut retained_count = 0;
        let mut retained = Vec::new();
        for _ in 0..super::super::MAX_RETAINED_FTS_HANDLES + 3 {
            if let Some(handle) =
                retain_completed_handle(&mut retained_count, DropSpy(drops.clone()))
            {
                retained.push(handle);
            }
        }
        assert_eq!(retained.len(), super::super::MAX_RETAINED_FTS_HANDLES);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        drop(retained);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            super::super::MAX_RETAINED_FTS_HANDLES + 3
        );
    }

    #[test]
    fn dedup_targets_collapses_repeated_repos() {
        let target = |id, qualifier: &str| {
            SearchTarget::new(
                id,
                RepoQualifier::new(qualifier.to_string()),
                ShardRevision::new("rev".to_string()),
            )
        };
        let targets = dedup_targets(vec![
            target(1, "a"),
            target(2, "b"),
            target(1, "a"),
            target(3, "c"),
        ]);
        assert_eq!(
            targets
                .iter()
                .map(|t| t.qualifier.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn parse_search_kinds_validates_the_vocabulary() {
        assert!(parse_search_kinds(None).unwrap().is_none());
        assert!(parse_search_kinds(Some(&[])).is_err());
        let error = parse_search_kinds(Some(&["Frobnicate".to_string()])).unwrap_err();
        assert!(error.contains("Frobnicate") && error.contains("Symbol"));
        assert_eq!(
            parse_search_kinds(Some(&["Symbol".to_string(), "File".to_string()]))
                .unwrap()
                .unwrap(),
            vec![yg_shard::NodeKind::Symbol, yg_shard::NodeKind::File]
        );
    }

    #[test]
    fn search_shard_errors_map_to_client_statuses() {
        let missing = || ResolveError::RevisionMissing(anyhow::anyhow!("gone"));
        assert!(matches!(
            map_search_shard_error(missing(), true),
            VerbError::Gone(_)
        ));
        assert!(matches!(
            map_search_shard_error(ResolveError::SchemaOutdated, true),
            VerbError::Gone(_)
        ));
        assert!(matches!(
            map_search_shard_error(ResolveError::SchemaOutdated, false),
            VerbError::Unavailable(_)
        ));
        assert!(matches!(
            map_search_shard_error(missing(), false),
            VerbError::Internal(_)
        ));
    }

    struct CountingResolver {
        target_count: usize,
        active: AtomicUsize,
        peak: AtomicUsize,
        resolutions: AtomicUsize,
    }

    impl CountingResolver {
        fn new(target_count: usize) -> Self {
            Self {
                target_count,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                resolutions: AtomicUsize::new(0),
            }
        }

        fn target(position: usize) -> SearchTarget {
            SearchTarget::new(
                position as i64,
                RepoQualifier::new(format!("repo-{position}")),
                ShardRevision::new("revision".to_string()),
            )
        }
    }

    impl ShardResolver for CountingResolver {
        async fn resolve(
            &self,
            _: &str,
            _: Option<String>,
        ) -> Result<crate::ResolvedShard, ResolveError> {
            Err(ResolveError::UnknownRepo)
        }

        async fn resolve_search_target(
            &self,
            qualifier: &RepoQualifier,
        ) -> Result<SearchTarget, ResolveError> {
            Ok(SearchTarget::new(
                1,
                qualifier.clone(),
                ShardRevision::new("revision".to_string()),
            ))
        }

        async fn indexed_search_targets(&self) -> Result<Vec<SearchTarget>, ResolveError> {
            Ok((0..self.target_count).map(Self::target).collect())
        }

        async fn resolve_fts(
            &self,
            target: &SearchTarget,
        ) -> Result<std::path::PathBuf, ResolveError> {
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(std::path::PathBuf::from(format!(
                "/definitely-missing/{}",
                target.repo_id()
            )))
        }
    }

    #[test]
    fn fresh_target_sets_are_capped_before_fanout() {
        let resolver = CountingResolver::new(super::super::MAX_SEARCH_TARGETS + 1);
        let error = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(resolve_search_targets(&resolver, None))
            .expect_err("oversized target set is refused");
        assert!(
            matches!(error, VerbError::BadRequest(message) if message.contains("1001 repositories"))
        );
    }

    #[test]
    fn fanout_never_exceeds_the_named_concurrency_bound() {
        let resolver = Arc::new(CountingResolver::new(
            super::super::MAX_CONCURRENT_SEARCH_FANOUT * 2,
        ));
        let targets = (0..resolver.target_count)
            .map(CountingResolver::target)
            .collect();
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(rank_targets(
                resolver.clone(),
                targets,
                "query".to_string(),
                None,
                false,
            ));
        assert!(matches!(result, Err(VerbError::Internal(_))));
        assert_eq!(
            resolver.peak.load(Ordering::SeqCst),
            super::super::MAX_CONCURRENT_SEARCH_FANOUT
        );
        assert_eq!(
            resolver.resolutions.load(Ordering::SeqCst),
            super::super::MAX_CONCURRENT_SEARCH_FANOUT,
            "a failing batch must not admit later targets"
        );
        assert_eq!(resolver.active.load(Ordering::SeqCst), 0);
    }
}
