//! The `search` Verb: lexical search fanned out over indexed repos.
//! Unlike the node-addressed Verbs (engine-owned, see `verbs`), search
//! execution still lives in the transport — it reads FTS segments, not
//! the graph — but its cursor goes through the engine's one codec.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::ApiError;
use crate::wire::{self, Wire, WireJson};

/// The deepest a `search` cursor may page. The window also bounds the
/// fan-out: each repo is asked for a constant top-[`MAX_SEARCH_WINDOW`]
/// hits (the deepest reachable page), which both keeps the merged ranking
/// stable across pages and caps the work an unbounded offset could demand.
const MAX_SEARCH_WINDOW: usize = 1000;

/// The most repositories one search may fan out over — the width bound a
/// cursor's (unsigned, hence untrusted) target list is held to, so a
/// tampered cursor can't amplify one request into unbounded segment opens.
/// Generous for M0; org-wide fan-out beyond this is the tiering work
/// deferred to M3 (RFC 0001 §11 Q5).
const MAX_SEARCH_TARGETS: usize = 1000;

/// One repo in a search's pinned fan-out set: queried at this exact
/// immutable revision, so every page of one search sees the same corpus
/// even across a pointer swap (only a fresh search picks up new Shards).
#[derive(Serialize, Deserialize, Clone)]
struct SearchTarget {
    repo_id: i64,
    qualifier: String,
    revision: String,
}

/// What a search `next_cursor` opaquely carries: the query state and the
/// pinned fan-out set, plus how many hits have already been returned.
/// Pages are recomputed from the top each time over the deterministic
/// merged ranking and sliced by `offset` — the same recompute-and-window
/// approach `neighbors` takes, here across repos instead of hops.
#[derive(Serialize, Deserialize)]
struct SearchCursor {
    query: String,
    kinds: Option<Vec<String>>,
    mode: String,
    targets: Vec<SearchTarget>,
    offset: usize,
}

/// One ranked hit: an external node id usable in `node`/`neighbors`, the
/// repo it came from, the score, and a highlighted snippet where the match
/// has one.
#[derive(Serialize)]
struct SearchHit {
    id: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    repo: String,
    #[serde(serialize_with = "wire::f32_shortest")]
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
    /// The Shard-internal id, kept off the wire — the snippet-hydration
    /// pass looks the hit up in its repo's segment by this.
    #[serde(skip)]
    local_id: String,
}

#[derive(Serialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
    next_cursor: Option<String>,
}

/// The page size to actually serve at `offset`: the requested `limit`,
/// clamped so the page never runs past the pagination window. A request
/// whose `limit` would overshoot the window gets a final short page rather
/// than a `next_cursor` the follow-up request would reject.
fn clamped_page_limit(offset: usize, limit: usize) -> usize {
    limit.min(MAX_SEARCH_WINDOW.saturating_sub(offset))
}

/// Merge per-repo hits into one ranking and return the page after
/// `offset`. The order is total and deterministic — score descending, then
/// repo qualifier, then id — so a cursor resumed against the same pinned
/// revisions and the same constant per-repo fetch sees exactly the pages
/// one uninterrupted read would have. `has_more` reports whether the
/// merged ranking holds anything past this page.
fn merge_paginate(mut all: Vec<SearchHit>, offset: usize, limit: usize) -> (Vec<SearchHit>, bool) {
    all.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.repo.cmp(&b.repo))
            .then_with(|| a.id.cmp(&b.id))
    });
    let has_more = all.len() > offset + limit;
    let page = all.into_iter().skip(offset).take(limit).collect();
    (page, has_more)
}

/// `POST /v1/verbs/search` (RFC 0001 §7): lexical search, fanned out over
/// the indexed repos, returning one ranked page with snippets and node ids.
pub(crate) async fn verb_search(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<yg_verbs::SearchRequest>,
) -> Result<Response, ApiError> {
    let _timer = state.engine.metrics().timer(yg_verbs::Verb::Search);
    let cursor = req
        .cursor
        .as_deref()
        .map(yg_verbs::cursor::decode::<SearchCursor>)
        .transpose()
        .map_err(ApiError::bad_request)?;

    // Page size may vary between pages; everything else is fixed at the
    // first page and carried by the cursor.
    let limit = req
        .limit
        .map_or(yg_verbs::DEFAULT_SEARCH_LIMIT, |l| l as usize);
    if !(yg_verbs::MIN_PAGE_LIMIT..=yg_verbs::MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(ApiError::bad_request(format!(
            "limit must be between {} and {}, got {limit}",
            yg_verbs::MIN_PAGE_LIMIT,
            yg_verbs::MAX_SEARCH_LIMIT
        )));
    }

    // The query state in force: the cursor's where present, the request's
    // on a fresh search.
    let (query, kind_strings, mode, targets, offset) = match cursor {
        Some(cursor) => {
            // The cursor pins the whole search — query, filters, and fan-out
            // set — so one cursor pages one fixed result set. A request may
            // re-send any of these alongside the cursor (a client that just
            // appends `cursor` to its last request), but a re-sent value that
            // *contradicts* the cursor is the client's error, not a silent
            // re-pin onto a different search. Mirrors `neighbors`'
            // shape-agreement check; `limit` is the one field free to vary.
            if let Some(q) = req.query.as_deref()
                && q.trim() != cursor.query
            {
                return Err(ApiError::bad_request(
                    "this cursor belongs to a different query; start a fresh search or \
                     pass the cursor without a query",
                ));
            }
            if let Some(mode) = &req.mode
                && mode != &cursor.mode
            {
                return Err(ApiError::bad_request(
                    "this cursor belongs to a different search mode; page it without a \
                     mode, or start a fresh search",
                ));
            }
            if let Some(kinds) = &req.kinds
                && str_set(kinds) != str_set(cursor.kinds.as_deref().unwrap_or_default())
            {
                return Err(ApiError::bad_request(
                    "this cursor belongs to a different kinds filter; page it without \
                     kinds, or start a fresh search",
                ));
            }
            if let Some(repos) = &req.repos
                && str_set(repos)
                    != cursor
                        .targets
                        .iter()
                        .map(|t| t.qualifier.as_str())
                        .collect()
            {
                return Err(ApiError::bad_request(
                    "this cursor belongs to a different repos filter; page it without \
                     repos, or start a fresh search",
                ));
            }
            // The cursor is unsigned, so its target list is untrusted: a
            // tampered cursor could repeat or pad it to fan a single
            // request out across an unbounded number of segment opens. Cap
            // and dedup it, mirroring the fresh path's `resolve_search_targets`.
            if cursor.targets.len() > MAX_SEARCH_TARGETS {
                return Err(ApiError::bad_request(
                    "invalid cursor: it names too many repositories",
                ));
            }
            (
                cursor.query,
                cursor.kinds,
                cursor.mode,
                dedup_targets(cursor.targets),
                cursor.offset,
            )
        }
        None => {
            let query = match req.query.as_deref().map(str::trim) {
                Some(q) if !q.is_empty() => q.to_string(),
                _ => return Err(ApiError::bad_request("search needs a non-empty query")),
            };
            let mode = req.mode.unwrap_or_else(|| "lexical".to_string());
            let targets = resolve_search_targets(&state, req.repos.as_deref()).await?;
            (query, req.kinds, mode, targets, 0)
        }
    };

    // The mode gate runs on the in-force value, so a forged or stale cursor
    // can't smuggle an unsupported mode past the fresh-search check.
    if mode != "lexical" {
        return Err(ApiError::bad_request(format!(
            "search mode {mode:?} is not available; only \"lexical\" is supported \
             (semantic and hybrid arrive with embeddings)"
        )));
    }

    // A cursor's offset is client-supplied (the cursor is unsigned), so
    // bound it before any arithmetic: past the window there is nothing to
    // serve, and an unbounded offset would overflow `offset + …` below.
    if offset >= MAX_SEARCH_WINDOW {
        return Err(ApiError::bad_request(format!(
            "cannot page beyond {MAX_SEARCH_WINDOW} results"
        )));
    }
    // Clamp this page to the window: a requested limit that would run past
    // it serves a final short page rather than stranding reachable hits
    // behind a cursor the next request would reject.
    let page_limit = clamped_page_limit(offset, limit);

    // Validate (and parse) the kind filter against the node-kind
    // vocabulary, so a typo errors instead of silently matching nothing.
    let kinds = parse_search_kinds(kind_strings.as_deref()).map_err(ApiError::bad_request)?;

    // No indexed repos at all (a fresh org-wide search before anything is
    // indexed): an empty result, not an error.
    if targets.is_empty() {
        return Ok(Wire(SearchResponse {
            hits: vec![],
            next_cursor: None,
        })
        .into_response());
    }

    // Each repo returns a *constant* top-K (the deepest reachable page),
    // independent of the offset. That keeps the merged ranking identical
    // across pages — a varying fetch would let tantivy's score-then-docid
    // truncation pick different tied hits per page, so a score tie at a
    // page boundary could drop or duplicate a hit. With a constant K the
    // global ranking is stable and offset-slicing is exact. A cursor pins
    // revisions, so a missing/outdated one is the cursor's to hear about.
    //
    // Cost note: this fans out *sequentially* and asks each repo for up to
    // K hits, so per-query work scales with the number of indexed repos —
    // fine at M0's scale, but the org-wide tiering and early termination an
    // org of thousands of repos needs are deliberately deferred to M3
    // (RFC 0001 §11 Q5). Sequential (not concurrent) is also the bound that
    // keeps a forged cursor's repo set from fanning out unboundedly at once.
    let fetch_each = MAX_SEARCH_WINDOW;
    let from_cursor = req.cursor.is_some();
    let mut all = Vec::new();
    for target in &targets {
        let dir = state
            .shards
            .fts_path(target.repo_id, &target.revision)
            .await
            .map_err(|e| map_search_shard_error(e, from_cursor))?;
        let query = query.clone();
        let kinds = kinds.clone();
        let qualifier = target.qualifier.clone();
        let hits = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<SearchHit>> {
            let index = yg_shard::open_fts(&dir)?;
            // Ranking only — snippets are hydrated below for the page that
            // survives the merge, never for every fetched candidate.
            let local = yg_shard::search(
                &index,
                &yg_shard::SearchParams {
                    query: &query,
                    kinds: kinds.as_deref(),
                    limit: fetch_each,
                },
            )?;
            Ok(local
                .into_iter()
                .map(|hit| qualify_hit(&qualifier, hit))
                .collect())
        })
        .await;
        match hits {
            Ok(Ok(hits)) => all.extend(hits),
            Ok(Err(e)) => return Err(map_search_query_error(e)),
            Err(e) => {
                return Err(ApiError::internal(
                    anyhow::Error::new(e).context("search task panicked"),
                ));
            }
        }
    }

    let (mut page, has_more) = merge_paginate(all, offset, page_limit);
    hydrate_snippets(&state, &targets, &query, from_cursor, &mut page).await?;
    // Offer a next page only while one is both available and reachable
    // (paging stops at the window).
    let next_cursor = (has_more && offset + page_limit < MAX_SEARCH_WINDOW).then(|| {
        yg_verbs::cursor::encode(&SearchCursor {
            query,
            kinds: kind_strings,
            mode,
            targets,
            offset: offset + page_limit,
        })
    });
    Ok(Wire(SearchResponse {
        hits: page,
        next_cursor,
    })
    .into_response())
}

/// Fill in the snippet of each hit on the final page, querying each repo's
/// segment once for just that page's hits — so snippet generation costs
/// scale with the page, not with everything the fan-out ranked. A repo's
/// segment is already warm from ranking, so this is a local reopen.
async fn hydrate_snippets(
    state: &AppState,
    targets: &[SearchTarget],
    query: &str,
    from_cursor: bool,
    page: &mut [SearchHit],
) -> Result<(), ApiError> {
    use std::collections::HashMap;
    let target_by_qualifier: HashMap<&str, &SearchTarget> =
        targets.iter().map(|t| (t.qualifier.as_str(), t)).collect();
    // Page-hit indices grouped by the repo they came from. Owned keys, so
    // this grouping doesn't borrow `page` while we write snippets back.
    let mut by_repo: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, hit) in page.iter().enumerate() {
        by_repo.entry(hit.repo.clone()).or_default().push(i);
    }
    for (qualifier, indices) in by_repo {
        let Some(target) = target_by_qualifier.get(qualifier.as_str()) else {
            continue;
        };
        let dir = state
            .shards
            .fts_path(target.repo_id, &target.revision)
            .await
            .map_err(|e| map_search_shard_error(e, from_cursor))?;
        let local_ids: Vec<String> = indices.iter().map(|&i| page[i].local_id.clone()).collect();
        let query = query.to_string();
        let snippets = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let index = yg_shard::open_fts(&dir)?;
            yg_shard::snippets_for(&index, &query, &local_ids)
        })
        .await;
        // Snippets are a non-critical enhancement: the ranked hits and their
        // node ids — the result that feeds node/neighbors — are already in
        // hand. A snippet-generation failure for one repo degrades that
        // repo's hits to no highlight rather than sinking the whole search.
        let snippets = match snippets {
            Ok(Ok(snippets)) => snippets,
            Ok(Err(e)) => {
                tracing::warn!(
                    "fts snippet generation failed; returning hits unhighlighted: {e:#}"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!("fts snippet task panicked; returning hits unhighlighted: {e}");
                continue;
            }
        };
        for &i in &indices {
            page[i].snippet = snippets.get(&page[i].local_id).cloned();
        }
    }
    Ok(())
}

/// Qualify one segment-local hit into a wire hit: the repo qualifier
/// spliced into the node id (RFC 0001 §5), so the id feeds straight into
/// `node`/`neighbors`.
fn qualify_hit(qualifier: &str, hit: yg_shard::LocalHit) -> SearchHit {
    let local_id = hit.node_id.clone();
    let id = yg_verbs::VerbId {
        repo: qualifier.to_string(),
        local: Some(hit.node_id),
    }
    .external();
    SearchHit {
        id,
        kind: hit.kind,
        name: hit.name,
        path: hit.path,
        repo: qualifier.to_string(),
        score: hit.score,
        snippet: hit.snippet,
        local_id,
    }
}

/// Parse and validate the `kinds` filter. An empty list is ambiguous (no
/// kinds vs no filter) and rejected; an unknown kind errors with the
/// vocabulary rather than silently matching nothing. A real kind that
/// carries no searchable text (Package, Commit, Contributor) is a valid
/// filter that simply matches nothing — forward-compatible with a pass
/// that later indexes it (the deliberate contract pinned by the
/// `a_search_filtered_to_an_unindexed_kind_is_empty_not_an_error` test).
fn parse_search_kinds(kinds: Option<&[String]>) -> Result<Option<Vec<yg_shard::NodeKind>>, String> {
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

/// Resolve the fan-out set: the named repos when `repos` is given (each
/// must be indexed), or every indexed repo otherwise. Errors come back as
/// ready-to-return responses — an unknown or unindexed named repo is the
/// client's to hear about.
async fn resolve_search_targets(
    state: &AppState,
    repos: Option<&[String]>,
) -> Result<Vec<SearchTarget>, ApiError> {
    let targets = match repos {
        Some(repos) => {
            // An explicit empty list is ambiguous (no repos vs no filter)
            // and rejected, mirroring the empty-`kinds` rule; omit `repos`
            // to search every indexed repo.
            if repos.is_empty() {
                return Err(ApiError::bad_request(
                    "repos must name at least one repository; omit it to search every indexed repo",
                ));
            }
            let mut targets = Vec::with_capacity(repos.len());
            let mut seen = std::collections::HashSet::new();
            for qualifier in repos {
                // A repo named twice is one target: otherwise its hits
                // would appear twice in the merged page.
                if !seen.insert(qualifier.as_str()) {
                    continue;
                }
                let target = state.control.verb_target(qualifier).await?.ok_or_else(|| {
                    ApiError::not_found(format!("no indexed repository matches {qualifier:?}"))
                })?;
                let Some(revision) = target.revision else {
                    return Err(ApiError::not_found(format!(
                        "{qualifier} is registered but not yet indexed; try again shortly"
                    )));
                };
                targets.push(SearchTarget {
                    repo_id: target.repo_id,
                    qualifier: qualifier.clone(),
                    revision,
                });
            }
            targets
        }
        None => state
            .control
            .indexed_repos(&yg_shard::syntactic_revision_suffix())
            .await?
            .into_iter()
            .map(|r| SearchTarget {
                repo_id: r.repo_id,
                qualifier: r.qualifier,
                revision: r.revision,
            })
            .collect(),
    };

    // The cursor that carries this fan-out is capped at MAX_SEARCH_TARGETS
    // so a tampered cursor can't fan out unboundedly; the fresh path must
    // honour the same bound, or a legitimately-minted cursor over a larger
    // fan-out would be rejected on its next page. Beyond the cap, search
    // needs a `repos` filter — org-wide tiering is M3 (RFC 0001 §11).
    if targets.len() > MAX_SEARCH_TARGETS {
        return Err(ApiError::bad_request(format!(
            "the search spans {} repositories, more than the {MAX_SEARCH_TARGETS} one query \
             covers; narrow it with the repos filter",
            targets.len()
        )));
    }
    Ok(targets)
}

/// The set of a list filter's values, for comparing a re-sent cursor filter
/// against the cursor's pinned one regardless of order or repeats.
fn str_set(items: &[String]) -> std::collections::HashSet<&str> {
    items.iter().map(String::as_str).collect()
}

/// Drop repeated repositories from a fan-out set, keeping first occurrence
/// order — a repo searched twice would otherwise return each hit twice in
/// the merged page. The fresh path dedups while resolving; this is the
/// same guarantee applied to a cursor's untrusted target list.
fn dedup_targets(targets: Vec<SearchTarget>) -> Vec<SearchTarget> {
    let mut seen = std::collections::HashSet::new();
    targets
        .into_iter()
        .filter(|t| seen.insert(t.qualifier.clone()))
        .collect()
}

/// Map a shard-resolution error from the search fan-out, the same way
/// the engine's resolver mapping does for the node-addressed Verbs.
fn map_search_shard_error(e: anyhow::Error, from_cursor: bool) -> ApiError {
    if from_cursor && e.downcast_ref::<yg_shard::RevisionMissing>().is_some() {
        return ApiError::gone(
            "this cursor's Shard revision is no longer available; restart the search without a cursor",
        );
    }
    if e.downcast_ref::<yg_shard::SchemaOutdated>().is_some() {
        return if from_cursor {
            ApiError::gone(
                "this cursor's Shard revision predates the current index schema; \
                 restart the search without a cursor",
            )
        } else {
            ApiError::unavailable(
                "a repo's Shard predates the current index schema and is being re-indexed; \
                 try again shortly",
            )
        };
    }
    ApiError::internal(e)
}

/// Map a search-execution error: a query tantivy can't parse is the
/// client's to fix (400); anything else is a 500.
fn map_search_query_error(e: anyhow::Error) -> ApiError {
    match e.downcast_ref::<yg_shard::QueryMalformed>() {
        Some(malformed) => ApiError::bad_request(malformed.to_string()),
        None => ApiError::internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn test_hit(repo: &str, id: &str, score: f32) -> SearchHit {
        SearchHit {
            id: id.to_string(),
            kind: "Symbol".to_string(),
            name: None,
            path: None,
            repo: repo.to_string(),
            score,
            snippet: None,
            local_id: id.to_string(),
        }
    }

    fn page_ids(page: Vec<SearchHit>) -> Vec<String> {
        page.into_iter().map(|h| h.id).collect()
    }

    /// Cross-repo merge: a total, deterministic order (score desc, then
    /// repo, then id) and exact offset paging — the contract the search
    /// cursor relies on to resume one consistent ranking.
    #[test]
    fn merge_paginate_orders_across_repos_and_pages_by_offset() {
        // Two repos' hits, interleaved by score, with a score tie that the
        // repo qualifier breaks ("a" before "b").
        let corpus = || {
            vec![
                test_hit("a", "sym:a:x#1", 1.0),
                test_hit("b", "sym:b:y#1", 3.0),
                test_hit("a", "sym:a:x#2", 2.0),
                test_hit("b", "sym:b:y#2", 2.0),
            ]
        };

        let (first, more) = merge_paginate(corpus(), 0, 2);
        assert_eq!(
            page_ids(first),
            ["sym:b:y#1", "sym:a:x#2"],
            "score desc, tie by repo"
        );
        assert!(more, "two hits remain");

        let (second, more) = merge_paginate(corpus(), 2, 2);
        assert_eq!(page_ids(second), ["sym:b:y#2", "sym:a:x#1"]);
        assert!(!more, "the ranking is exhausted");
    }

    /// The final tiebreak is the id: same score, same repo, ordered by id;
    /// and `has_more` is exact at the page boundary and past the end.
    #[test]
    fn merge_paginate_breaks_score_and_repo_ties_by_id_and_bounds_pages() {
        // All same score and repo: only the id tiebreak orders them.
        let corpus = || {
            vec![
                test_hit("a", "sym:a:c", 2.0),
                test_hit("a", "sym:a:a", 2.0),
                test_hit("a", "sym:a:b", 2.0),
            ]
        };

        let (page, more) = merge_paginate(corpus(), 0, 3);
        assert_eq!(
            page_ids(page),
            ["sym:a:a", "sym:a:b", "sym:a:c"],
            "the id breaks score+repo ties"
        );
        // The page consumes the whole ranking exactly: no next page.
        assert!(!more, "offset+limit == len is not 'more'");

        // A page that exactly reaches the end has no more.
        let (page, more) = merge_paginate(corpus(), 0, 2);
        assert_eq!(page_ids(page), ["sym:a:a", "sym:a:b"]);
        assert!(more, "one hit remains after a 2-of-3 page");

        // An offset past the end is an empty page, not an error.
        let (page, more) = merge_paginate(corpus(), 5, 2);
        assert!(page.is_empty(), "offset past the end yields nothing");
        assert!(!more);
    }

    /// A page is clamped to the pagination window so a `limit` that doesn't
    /// divide it never strands reachable hits behind a rejected cursor: the
    /// last page is short, and `offset + page_limit` lands exactly on the
    /// window so no further cursor is offered.
    #[test]
    fn page_limit_is_clamped_to_the_window() {
        assert_eq!(clamped_page_limit(0, 20), 20, "well inside the window");
        assert_eq!(
            clamped_page_limit(MAX_SEARCH_WINDOW - 40, 30),
            30,
            "a full page still fits"
        );
        // A limit overshooting the window is clamped to the remainder, and
        // the next offset lands on the window edge — no next page offered.
        let near = MAX_SEARCH_WINDOW - 10;
        let clamped = clamped_page_limit(near, 30);
        assert_eq!(clamped, 10, "clamped to the window remainder");
        assert_eq!(
            near + clamped,
            MAX_SEARCH_WINDOW,
            "the next offset is the edge"
        );
        // At the edge the remainder is zero (the handler rejects offset ==
        // window before this, so this is just the saturating boundary).
        assert_eq!(clamped_page_limit(MAX_SEARCH_WINDOW, 30), 0);
    }

    /// A cursor's (untrusted) target list is deduped by qualifier, keeping
    /// first-seen order — so a tampered cursor repeating a repo can't
    /// double its hits or fan the request out redundantly.
    #[test]
    fn dedup_targets_collapses_repeated_repos() {
        let target = |id: i64, q: &str| SearchTarget {
            repo_id: id,
            qualifier: q.to_string(),
            revision: "rev".to_string(),
        };
        let deduped = dedup_targets(vec![
            target(1, "a"),
            target(2, "b"),
            target(1, "a"),
            target(1, "a"),
            target(3, "c"),
        ]);
        let qualifiers: Vec<&str> = deduped.iter().map(|t| t.qualifier.as_str()).collect();
        assert_eq!(qualifiers, ["a", "b", "c"], "repeats dropped, order kept");
    }

    /// Shard-resolution errors during search map to the same client
    /// statuses the node-addressed Verbs use: a cursor outliving its Shard
    /// is the client's to restart (410), a re-indexing Shard is a retry
    /// (503).
    #[test]
    fn search_shard_errors_map_to_client_statuses() {
        let missing = || {
            anyhow::Error::new(yg_shard::RevisionMissing {
                revision: "r".to_string(),
            })
        };
        let outdated = || {
            anyhow::Error::new(yg_shard::SchemaOutdated {
                revision: "r".to_string(),
                schema_version: 1,
            })
        };
        assert_eq!(
            map_search_shard_error(missing(), true).status,
            StatusCode::GONE
        );
        assert_eq!(
            map_search_shard_error(outdated(), true).status,
            StatusCode::GONE
        );
        assert_eq!(
            map_search_shard_error(outdated(), false).status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        // A fresh search resolves a current pointer, so a missing revision
        // there is an unexpected server fault, not a client-expired cursor.
        let internal = map_search_shard_error(missing(), false);
        assert_eq!(internal.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            internal.message, "internal server error",
            "500s carry no error-chain content"
        );
    }

    /// The `kinds` filter is validated against the node-kind vocabulary:
    /// an empty list is ambiguous, an unknown kind names the vocabulary.
    #[test]
    fn parse_search_kinds_validates_the_vocabulary() {
        assert!(parse_search_kinds(None).unwrap().is_none(), "no filter");
        assert!(
            parse_search_kinds(Some(&[])).is_err(),
            "an empty list is ambiguous"
        );
        let err = parse_search_kinds(Some(&["Frobnicate".to_string()])).unwrap_err();
        assert!(
            err.contains("Frobnicate") && err.contains("Symbol"),
            "names the typo and the vocabulary: {err}"
        );
        let ok = parse_search_kinds(Some(&["Symbol".to_string(), "File".to_string()]))
            .unwrap()
            .unwrap();
        assert_eq!(
            ok,
            vec![yg_shard::NodeKind::Symbol, yg_shard::NodeKind::File]
        );
    }
}
