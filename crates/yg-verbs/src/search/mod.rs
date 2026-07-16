//! Engine-owned orchestration for the `search` Verb.

use std::sync::Arc;

use crate::cursor::{CursorCodec, SearchCursor, SearchKind, SearchMode};
use crate::engine::{ShardResolver, VerbError};
use crate::{DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT, MIN_PAGE_LIMIT, SearchRequest};

mod fanout;
mod hydrate;
mod merge;
mod types;

pub use types::{
    RepoQualifier, SearchHit, SearchNodeName, SearchPath, SearchResponse, SearchSnippet,
    SearchTarget, SearchTargetProvenance, SearchWireResponse, ShardRevision,
};

/// The deepest a search cursor may page and each repository may rank.
const MAX_SEARCH_WINDOW: usize = 1000;

/// The most repositories one search may fan out over.
const MAX_SEARCH_TARGETS: usize = 1000;

/// Maximum number of repository indexes opened and ranked at once.
const MAX_CONCURRENT_SEARCH_FANOUT: usize = 8;

/// Run `search` end to end behind the engine seam.
pub(crate) async fn search<R: ShardResolver + 'static>(
    resolver: Arc<R>,
    cursors: &CursorCodec,
    req: SearchRequest,
) -> Result<SearchResponse, VerbError> {
    let cursor = req
        .cursor
        .as_deref()
        .map(|cursor| cursors.decode::<SearchCursor>(cursor))
        .transpose()
        .map_err(VerbError::InvalidCursor)?;
    let limit = req
        .limit
        .map_or(DEFAULT_SEARCH_LIMIT, |limit| limit as usize);
    if !(MIN_PAGE_LIMIT..=MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(VerbError::BadRequest(format!(
            "limit must be between {MIN_PAGE_LIMIT} and {MAX_SEARCH_LIMIT}, got {limit}"
        )));
    }

    let (query, kind_values, mode, targets, offset, target_provenance) = match cursor {
        Some(cursor) => {
            cursor
                .agrees_with(&req, MAX_SEARCH_TARGETS)
                .map_err(VerbError::InvalidCursor)?;
            (
                cursor.query,
                cursor.kinds,
                cursor.mode,
                fanout::dedup_targets(cursor.targets),
                cursor.offset,
                SearchTargetProvenance::ResumedFromCursor,
            )
        }
        None => {
            let query = match req.query.as_deref().map(str::trim) {
                Some(query) if !query.is_empty() => query.to_string(),
                _ => {
                    return Err(VerbError::BadRequest(
                        "search needs a non-empty query".to_string(),
                    ));
                }
            };
            let mode = SearchMode::from(req.mode.unwrap_or_else(|| "lexical".to_string()));
            let targets =
                fanout::resolve_search_targets(resolver.as_ref(), req.repos.as_deref()).await?;
            let kinds = req
                .kinds
                .map(|kinds| kinds.into_iter().map(SearchKind::from).collect());
            (
                query,
                kinds,
                mode,
                targets,
                0,
                SearchTargetProvenance::FreshlyEnumerated,
            )
        }
    };

    if mode != "lexical" {
        return Err(VerbError::BadRequest(format!(
            "search mode {mode:?} is not available; only \"lexical\" is supported (semantic and hybrid arrive with embeddings)"
        )));
    }
    if offset >= MAX_SEARCH_WINDOW {
        return Err(VerbError::BadRequest(format!(
            "cannot page beyond {MAX_SEARCH_WINDOW} results"
        )));
    }
    let page_limit = merge::clamped_page_limit(offset, limit);
    let kind_strings = kind_values.as_ref().map(|kinds| {
        kinds
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect::<Vec<_>>()
    });
    let kinds =
        fanout::parse_search_kinds(kind_strings.as_deref()).map_err(VerbError::BadRequest)?;
    if targets.is_empty() {
        return Ok(SearchResponse {
            hits: vec![],
            next: None,
        });
    }

    let mut ranked = fanout::rank_targets(
        resolver.clone(),
        targets.clone(),
        query.clone(),
        kinds,
        target_provenance,
    )
    .await?;
    let all = ranked
        .iter_mut()
        .flat_map(|repo| std::mem::take(&mut repo.hits))
        .collect();
    let (mut page, has_more) = merge::merge_paginate(all, offset, page_limit);
    let page_repos = hydrate::retain_page_repos(ranked, &page);
    hydrate::hydrate_snippets(resolver, page_repos, target_provenance, &query, &mut page).await;

    let next = (has_more && offset + page_limit < MAX_SEARCH_WINDOW).then(|| SearchCursor {
        query,
        kinds: kind_values,
        mode,
        targets,
        offset: offset + page_limit,
    });
    Ok(SearchResponse { hits: page, next })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cursors() -> CursorCodec {
        CursorCodec::new(
            crate::CursorSecret::new(b"search-module-test-secret-at-least-32-bytes".to_vec())
                .expect("test secret is non-empty"),
        )
    }

    struct EmptyResolver;

    impl ShardResolver for EmptyResolver {
        async fn resolve(
            &self,
            _: &str,
            _: Option<crate::PinnedShard>,
        ) -> Result<crate::ResolvedShard, crate::ResolveError> {
            Err(crate::ResolveError::UnknownRepo)
        }

        async fn resolve_search_target(
            &self,
            _: &RepoQualifier,
        ) -> Result<SearchTarget, crate::ResolveError> {
            Err(crate::ResolveError::UnknownRepo)
        }

        async fn indexed_search_targets(&self) -> Result<Vec<SearchTarget>, crate::ResolveError> {
            Ok(vec![])
        }

        async fn resolve_fts(
            &self,
            _: &SearchTarget,
            provenance: SearchTargetProvenance,
        ) -> Result<crate::ResolvedFts, crate::ResolveError> {
            assert_eq!(provenance, SearchTargetProvenance::ResumedFromCursor);
            Err(crate::ResolveError::RevisionMissing(anyhow::anyhow!(
                "cursor target vanished"
            )))
        }
    }

    fn resume(cursor: String) -> Result<SearchResponse, VerbError> {
        let cursors = test_cursors();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(search(
                Arc::new(EmptyResolver),
                &cursors,
                SearchRequest {
                    query: None,
                    kinds: None,
                    repos: None,
                    mode: None,
                    limit: None,
                    cursor: Some(cursor),
                },
            ))
    }

    #[test]
    fn cursor_target_cap_applies_before_deduplication() {
        let repeated = SearchTarget::new(
            1,
            RepoQualifier::new("repo".to_string()),
            ShardRevision::new("revision".to_string()),
        );
        let cursor = test_cursors().encode(&SearchCursor {
            query: "query".to_string(),
            kinds: None,
            mode: SearchMode::from("lexical".to_string()),
            targets: vec![repeated; MAX_SEARCH_TARGETS + 1],
            offset: 0,
        });
        let outcome = resume(cursor);
        assert!(matches!(
            outcome,
            Err(VerbError::InvalidCursor(
                crate::CursorError::SearchTargetCap
            ))
        ));
    }

    #[test]
    fn unknown_cursor_mode_reaches_the_existing_mode_gate() {
        let cursor = test_cursors().encode(&SearchCursor {
            query: "query".to_string(),
            kinds: None,
            mode: SearchMode::from("future-mode".to_string()),
            targets: vec![],
            offset: 0,
        });

        let outcome = resume(cursor);
        assert!(
            matches!(outcome, Err(VerbError::BadRequest(message)) if message ==
                "search mode \"future-mode\" is not available; only \"lexical\" is supported (semantic and hybrid arrive with embeddings)")
        );
    }

    #[test]
    fn vanished_cursor_target_is_gone_not_internal() {
        let cursor = test_cursors().encode(&SearchCursor {
            query: "query".to_string(),
            kinds: None,
            mode: SearchMode::Lexical,
            targets: vec![SearchTarget::new(
                1,
                RepoQualifier::new("vanished".to_string()),
                ShardRevision::new("revision".to_string()),
            )],
            offset: 0,
        });

        assert!(matches!(resume(cursor), Err(VerbError::Gone(_))));
    }

    #[test]
    fn unknown_cursor_kind_reaches_the_existing_vocabulary_gate() {
        let cursor = test_cursors().encode(&SearchCursor {
            query: "query".to_string(),
            kinds: Some(vec![SearchKind::from("FutureKind".to_string())]),
            mode: SearchMode::Lexical,
            targets: vec![],
            offset: 0,
        });

        let outcome = resume(cursor);
        assert!(
            matches!(outcome, Err(VerbError::BadRequest(message)) if message ==
                "unknown node kind \"FutureKind\": expected any of File, Symbol, Package, Commit, Contributor")
        );
    }
}
