//! Engine-owned orchestration for the `search` Verb: cursor policy,
//! target resolution, bounded fan-out, deterministic merge, and snippets.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::cursor::{self, SearchCursor};
use crate::engine::{ResolveError, ShardResolver, VerbError};
use crate::{DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT, MIN_PAGE_LIMIT, SearchRequest, VerbId};

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

/// Maximum number of repository indexes opened and ranked at once.
/// This bounds blocking-pool pressure while allowing independent Shards
/// to search concurrently.
const MAX_CONCURRENT_SEARCH_FANOUT: usize = 8;

/// A typed repository qualifier on the search resolver seam.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoQualifier(String);

impl RepoQualifier {
    /// Wrap a qualifier parsed by the deployment's control-plane boundary.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The qualifier's canonical wire spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A typed immutable Shard revision on the search resolver seam.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ShardRevision(String);

impl ShardRevision {
    /// Wrap an immutable revision parsed by the deployment boundary.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The revision's canonical storage spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One repo in a search's pinned fan-out set: queried at this exact
/// immutable revision, so every page of one search sees the same corpus
/// even across a pointer swap (only a fresh search picks up new Shards).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SearchTarget {
    repo_id: i64,
    qualifier: RepoQualifier,
    revision: ShardRevision,
}

impl SearchTarget {
    /// Build one pinned search target from typed resolver values.
    pub fn new(repo_id: i64, qualifier: RepoQualifier, revision: ShardRevision) -> Self {
        Self {
            repo_id,
            qualifier,
            revision,
        }
    }

    /// The control-plane repository identifier used by the segment cache.
    pub fn repo_id(&self) -> i64 {
        self.repo_id
    }

    /// The qualifier prepended to this repository's local node ids.
    pub fn qualifier(&self) -> &RepoQualifier {
        &self.qualifier
    }

    /// The immutable Shard revision pinned by the search cursor.
    pub fn revision(&self) -> &ShardRevision {
        &self.revision
    }
}

/// A node display name returned by search.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct SearchNodeName(String);

impl SearchNodeName {
    fn new(value: String) -> Self {
        Self(value)
    }

    /// The node name as indexed.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A repository-relative node path returned by search.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct SearchPath(String);

impl SearchPath {
    fn new(value: String) -> Self {
        Self(value)
    }

    /// The repository-relative path spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A highlighted search excerpt returned for a content hit.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct SearchSnippet(String);

impl SearchSnippet {
    fn new(value: String) -> Self {
        Self(value)
    }

    /// The highlighted HTML excerpt.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One ranked hit: an external node id usable in `node`/`neighbors`, the
/// repo it came from, the score, and a highlighted snippet where the match
/// has one.
#[derive(Debug, Serialize)]
pub struct SearchHit {
    #[serde(serialize_with = "serialize_verb_id")]
    pub id: VerbId,
    #[serde(serialize_with = "serialize_node_kind")]
    pub kind: yg_shard::NodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<SearchNodeName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<SearchPath>,
    pub repo: RepoQualifier,
    #[serde(serialize_with = "f32_shortest")]
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<SearchSnippet>,
}

#[derive(Debug)]
pub struct SearchResponse {
    /// The requested page in deterministic merged rank order.
    pub hits: Vec<SearchHit>,
    /// Typed continuation for the next page, or `None` when exhausted.
    pub next: Option<SearchCursor>,
}

fn serialize_verb_id<S: serde::Serializer>(id: &VerbId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.external())
}

fn serialize_node_kind<S: serde::Serializer>(
    kind: &yg_shard::NodeKind,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(kind.as_str())
}

/// Preserve the existing shortest-f32 wire representation when canonical
/// JSON first converts this response through `serde_json::Value`.
fn f32_shortest<S: serde::Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
    let shortest: f64 = value
        .to_string()
        .parse()
        .unwrap_or_else(|_| f64::from(*value));
    serializer.serialize_f64(shortest)
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
fn merge_paginate(all: Vec<SearchHit>, offset: usize, limit: usize) -> (Vec<SearchHit>, bool) {
    let mut keyed: Vec<_> = all
        .into_iter()
        .map(|hit| (hit.id.external(), hit))
        .collect();
    keyed.sort_by(|(a_id, a), (b_id, b)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.repo.as_str().cmp(b.repo.as_str()))
            .then_with(|| a_id.cmp(b_id))
    });
    let has_more = keyed.len() > offset + limit;
    let page = keyed
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, hit)| hit)
        .collect();
    (page, has_more)
}

/// Run `search` end to end behind the engine seam.
pub(crate) async fn search<R: ShardResolver + 'static>(
    resolver: Arc<R>,
    req: SearchRequest,
) -> Result<SearchResponse, VerbError> {
    let cursor = req
        .cursor
        .as_deref()
        .map(cursor::decode::<SearchCursor>)
        .transpose()
        .map_err(VerbError::BadRequest)?;

    // Page size may vary between pages; everything else is fixed at the
    // first page and carried by the cursor.
    let limit = req.limit.map_or(DEFAULT_SEARCH_LIMIT, |l| l as usize);
    if !(MIN_PAGE_LIMIT..=MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(VerbError::BadRequest(format!(
            "limit must be between {MIN_PAGE_LIMIT} and {MAX_SEARCH_LIMIT}, got {limit}"
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
            cursor.agrees_with(&req).map_err(VerbError::BadRequest)?;
            // The cursor is unsigned, so its target list is untrusted: a
            // tampered cursor could repeat or pad it to fan a single
            // request out across an unbounded number of segment opens. Cap
            // and dedup it, mirroring the fresh path's `resolve_search_targets`.
            if cursor.targets.len() > MAX_SEARCH_TARGETS {
                return Err(VerbError::BadRequest(
                    "invalid cursor: it names too many repositories".to_string(),
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
                _ => {
                    return Err(VerbError::BadRequest(
                        "search needs a non-empty query".to_string(),
                    ));
                }
            };
            let mode = req.mode.unwrap_or_else(|| "lexical".to_string());
            let targets = resolve_search_targets(resolver.as_ref(), req.repos.as_deref()).await?;
            (query, req.kinds, mode, targets, 0)
        }
    };

    // The mode gate runs on the in-force value, so a forged or stale cursor
    // can't smuggle an unsupported mode past the fresh-search check.
    if mode != "lexical" {
        return Err(VerbError::BadRequest(format!(
            "search mode {mode:?} is not available; only \"lexical\" is supported \
             (semantic and hybrid arrive with embeddings)"
        )));
    }

    // A cursor's offset is client-supplied (the cursor is unsigned), so
    // bound it before any arithmetic: past the window there is nothing to
    // serve, and an unbounded offset would overflow `offset + …` below.
    if offset >= MAX_SEARCH_WINDOW {
        return Err(VerbError::BadRequest(format!(
            "cannot page beyond {MAX_SEARCH_WINDOW} results"
        )));
    }
    // Clamp this page to the window: a requested limit that would run past
    // it serves a final short page rather than stranding reachable hits
    // behind a cursor the next request would reject.
    let page_limit = clamped_page_limit(offset, limit);

    // Validate (and parse) the kind filter against the node-kind
    // vocabulary, so a typo errors instead of silently matching nothing.
    let kinds = parse_search_kinds(kind_strings.as_deref()).map_err(VerbError::BadRequest)?;

    // No indexed repos at all (a fresh org-wide search before anything is
    // indexed): an empty result, not an error.
    if targets.is_empty() {
        return Ok(SearchResponse {
            hits: vec![],
            next: None,
        });
    }

    // Each repo returns a *constant* top-K (the deepest reachable page),
    // independent of the offset. That keeps the merged ranking identical
    // across pages — a varying fetch would let tantivy's score-then-docid
    // truncation pick different tied hits per page, so a score tie at a
    // page boundary could drop or duplicate a hit. With a constant K the
    // global ranking is stable and offset-slicing is exact. A cursor pins
    // revisions, so a missing/outdated one is the cursor's to hear about.
    //
    let from_cursor = req.cursor.is_some();
    let ranked = rank_targets(resolver, targets.clone(), query.clone(), kinds, from_cursor).await?;
    let mut all = Vec::new();
    let mut indexes = Vec::with_capacity(ranked.len());
    for RankedRepo {
        qualifier,
        index,
        hits,
    } in ranked
    {
        all.extend(hits);
        indexes.push((qualifier, index));
    }

    let (mut page, has_more) = merge_paginate(all, offset, page_limit);
    let indexes = retain_page_indexes(indexes, &page);
    hydrate_snippets(indexes, &query, &mut page).await;
    // Offer a next page only while one is both available and reachable
    // (paging stops at the window).
    let next = (has_more && offset + page_limit < MAX_SEARCH_WINDOW).then(|| SearchCursor {
        query,
        kinds: kind_strings,
        mode,
        targets,
        offset: offset + page_limit,
    });
    Ok(SearchResponse { hits: page, next })
}

struct RankedRepo {
    qualifier: RepoQualifier,
    index: yg_shard::FtsIndex,
    hits: Vec<SearchHit>,
}

/// Resolve, open, and rank repositories concurrently while never placing
/// more than [`MAX_CONCURRENT_SEARCH_FANOUT`] jobs on the async/blocking
/// runtimes. Results are restored to target order before errors are read;
/// merge order itself is independent because its sort is total.
async fn rank_targets<R: ShardResolver + 'static>(
    resolver: Arc<R>,
    targets: Vec<SearchTarget>,
    query: String,
    kinds: Option<Vec<yg_shard::NodeKind>>,
    from_cursor: bool,
) -> Result<Vec<RankedRepo>, VerbError> {
    let total = targets.len();
    let mut targets = targets.into_iter().enumerate();
    let mut ranked = Vec::with_capacity(total);

    loop {
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..MAX_CONCURRENT_SEARCH_FANOUT {
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
            batch.push(joined.map_err(|error| {
                VerbError::Internal(anyhow::Error::new(error).context("search task panicked"))
            })?);
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
                    limit: MAX_SEARCH_WINDOW,
                },
            )?;
            let hits = local
                .into_iter()
                .map(|hit| qualify_hit(qualifier.as_str(), hit))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(RankedRepo {
                qualifier,
                index,
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

/// Keep only the index handles needed to hydrate the final page, dropping
/// every non-page repository before snippet hydration starts.
fn retain_page_indexes<I>(
    mut indexes: Vec<(RepoQualifier, I)>,
    page: &[SearchHit],
) -> Vec<(RepoQualifier, I)> {
    let page_repos: HashSet<&RepoQualifier> = page.iter().map(|hit| &hit.repo).collect();
    indexes.retain(|(qualifier, _)| page_repos.contains(qualifier));
    indexes
}

async fn hydrate_snippets(
    indexes: Vec<(RepoQualifier, yg_shard::FtsIndex)>,
    query: &str,
    page: &mut [SearchHit],
) {
    use std::collections::HashMap;
    let mut by_repo: HashMap<RepoQualifier, Vec<usize>> = HashMap::new();
    for (i, hit) in page.iter().enumerate() {
        by_repo.entry(hit.repo.clone()).or_default().push(i);
    }
    for (qualifier, index) in indexes {
        let Some(indices) = by_repo.remove(&qualifier) else {
            continue;
        };
        let local_ids: Vec<String> = indices
            .iter()
            .map(|&i| {
                page[i]
                    .id
                    .local
                    .clone()
                    .expect("a search hit always has a segment-local id")
            })
            .collect();
        let query = query.to_string();
        let snippets = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
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
            let local_id = page[i]
                .id
                .local
                .as_ref()
                .expect("a search hit always has a segment-local id");
            page[i].snippet = snippets.get(local_id).cloned().map(SearchSnippet::new);
        }
    }
}

/// Qualify one segment-local hit into a wire hit: the repo qualifier
/// spliced into the node id (RFC 0001 §5), so the id feeds straight into
/// `node`/`neighbors`.
fn qualify_hit(qualifier: &str, hit: yg_shard::LocalHit) -> anyhow::Result<SearchHit> {
    let kind = yg_shard::NodeKind::parse(&hit.kind)
        .with_context(|| format!("an FTS hit has unknown node kind {:?}", hit.kind))?;
    Ok(SearchHit {
        id: VerbId {
            repo: qualifier.to_string(),
            local: Some(hit.node_id),
        },
        kind,
        name: hit.name.map(SearchNodeName::new),
        path: hit.path.map(SearchPath::new),
        repo: RepoQualifier::new(qualifier.to_string()),
        score: hit.score,
        snippet: hit.snippet.map(SearchSnippet::new),
    })
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
    resolver: &impl ShardResolver,
    repos: Option<&[String]>,
) -> Result<Vec<SearchTarget>, VerbError> {
    let targets = match repos {
        Some(repos) => {
            // An explicit empty list is ambiguous (no repos vs no filter)
            // and rejected, mirroring the empty-`kinds` rule; omit `repos`
            // to search every indexed repo.
            if repos.is_empty() {
                return Err(VerbError::BadRequest(
                    "repos must name at least one repository; omit it to search every indexed repo"
                        .to_string(),
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

    // The cursor that carries this fan-out is capped at MAX_SEARCH_TARGETS
    // so a tampered cursor can't fan out unboundedly; the fresh path must
    // honour the same bound, or a legitimately-minted cursor over a larger
    // fan-out would be rejected on its next page. Beyond the cap, search
    // needs a `repos` filter — org-wide tiering is M3 (RFC 0001 §11).
    if targets.len() > MAX_SEARCH_TARGETS {
        return Err(VerbError::BadRequest(format!(
            "the search spans {} repositories, more than the {MAX_SEARCH_TARGETS} one query \
             covers; narrow it with the repos filter",
            targets.len()
        )));
    }
    Ok(targets)
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

fn map_named_target_error(qualifier: &str, error: ResolveError) -> VerbError {
    match error {
        ResolveError::UnknownRepo => {
            VerbError::NotFound(format!("no indexed repository matches {qualifier:?}"))
        }
        ResolveError::NotIndexed => VerbError::NotFound(format!(
            "{qualifier} is registered but not yet indexed; try again shortly"
        )),
        ResolveError::RevisionMissing(source) | ResolveError::Internal(source) => {
            VerbError::Internal(source)
        }
        ResolveError::SchemaOutdated => VerbError::Unavailable(
            "a repo's Shard predates the current index schema and is being re-indexed; \
             try again shortly"
                .to_string(),
        ),
    }
}

/// Map a shard-resolution error from the search fan-out, the same way
/// the engine's resolver mapping does for the node-addressed Verbs.
fn map_search_shard_error(error: ResolveError, from_cursor: bool) -> VerbError {
    match error {
        ResolveError::RevisionMissing(_) if from_cursor => VerbError::Gone(
            "this cursor's Shard revision is no longer available; restart the search without a cursor"
                .to_string(),
        ),
        ResolveError::SchemaOutdated if from_cursor => VerbError::Gone(
            "this cursor's Shard revision predates the current index schema; \
             restart the search without a cursor"
                .to_string(),
        ),
        ResolveError::SchemaOutdated => VerbError::Unavailable(
            "a repo's Shard predates the current index schema and is being re-indexed; \
             try again shortly"
                .to_string(),
        ),
        ResolveError::RevisionMissing(source) | ResolveError::Internal(source) => {
            VerbError::Internal(source)
        }
        ResolveError::UnknownRepo => VerbError::NotFound(
            "no indexed repository matches the search target".to_string(),
        ),
        ResolveError::NotIndexed => VerbError::NotFound(
            "a search target is registered but not yet indexed; try again shortly".to_string(),
        ),
    }
}

/// Map a search-execution error: a query tantivy can't parse is the
/// client's to fix (400); anything else is a 500.
fn map_search_query_error(e: anyhow::Error) -> VerbError {
    match e.downcast_ref::<yg_shard::QueryMalformed>() {
        Some(malformed) => VerbError::BadRequest(malformed.to_string()),
        None => VerbError::Internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_hit(repo: &str, id: &str, score: f32) -> SearchHit {
        SearchHit {
            id: VerbId::parse(id).expect("test id parses"),
            kind: yg_shard::NodeKind::Symbol,
            name: None,
            path: None,
            repo: RepoQualifier::new(repo.to_string()),
            score,
            snippet: None,
        }
    }

    fn page_ids(page: Vec<SearchHit>) -> Vec<String> {
        page.into_iter().map(|hit| hit.id.external()).collect()
    }

    #[test]
    fn typed_hits_keep_the_existing_wire_scalars() {
        let hit = qualify_hit(
            "github.com/acme/widgets",
            yg_shard::LocalHit {
                node_id: "sym:main.go#RateLimit".to_string(),
                kind: "Symbol".to_string(),
                name: Some("RateLimit".to_string()),
                path: Some("main.go".to_string()),
                score: 5.480_152,
                snippet: Some("<b>RateLimit</b>".to_string()),
            },
        )
        .expect("fixture hit qualifies");
        assert_eq!(
            serde_json::to_string(&hit).expect("serializes"),
            r#"{"id":"sym:github.com/acme/widgets:main.go#RateLimit","kind":"Symbol","name":"RateLimit","path":"main.go","repo":"github.com/acme/widgets","score":5.480152,"snippet":"<b>RateLimit</b>"}"#
        );
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

    #[test]
    fn merge_paginate_preserves_external_id_order_across_node_kinds() {
        let corpus = vec![
            test_hit("a", "sym:a:z", 2.0),
            test_hit("a", "repo:a", 2.0),
            test_hit("a", "pkg:a:z", 2.0),
        ];

        let (page, more) = merge_paginate(corpus, 0, 3);
        assert_eq!(page_ids(page), ["pkg:a:z", "repo:a", "sym:a:z"]);
        assert!(!more);
    }

    #[test]
    fn non_page_indexes_drop_before_hydration() {
        struct DropSpy(Arc<AtomicUsize>);

        impl Drop for DropSpy {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let page = vec![test_hit("page", "sym:page:x", 1.0)];
        let indexes = vec![
            (
                RepoQualifier::new("page".to_string()),
                DropSpy(drops.clone()),
            ),
            (
                RepoQualifier::new("off-page".to_string()),
                DropSpy(drops.clone()),
            ),
        ];

        let indexes = retain_page_indexes(indexes, &page);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the off-page handle drops before hydration can start"
        );
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].0.as_str(), "page");

        drop(indexes);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
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
        let target = |id: i64, qualifier: &str| {
            SearchTarget::new(
                id,
                RepoQualifier::new(qualifier.to_string()),
                ShardRevision::new("rev".to_string()),
            )
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
        let missing = || ResolveError::RevisionMissing(anyhow::anyhow!("gone from storage"));
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
        // A fresh search resolves a current pointer, so a missing revision
        // there is an unexpected server fault, not a client-expired cursor.
        let internal = map_search_shard_error(missing(), false);
        assert!(matches!(internal, VerbError::Internal(_)));
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

    struct CountingResolver {
        target_count: usize,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        resolutions: Arc<AtomicUsize>,
        fts_path: Option<std::path::PathBuf>,
    }

    impl CountingResolver {
        fn new(target_count: usize) -> Self {
            Self {
                target_count,
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                resolutions: Arc::new(AtomicUsize::new(0)),
                fts_path: None,
            }
        }

        fn with_fts_path(path: std::path::PathBuf) -> Self {
            Self {
                fts_path: Some(path),
                ..Self::new(1)
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
            _qualifier: &str,
            _pinned: Option<String>,
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
            Ok(self.fts_path.clone().unwrap_or_else(|| {
                std::path::PathBuf::from(format!("/definitely-missing/{}", target.repo_id()))
            }))
        }
    }

    #[test]
    fn fresh_target_sets_are_capped_before_fanout() {
        let resolver = CountingResolver::new(MAX_SEARCH_TARGETS + 1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(resolve_search_targets(&resolver, None))
            .expect_err("an oversized target set is refused");
        assert!(
            matches!(error, VerbError::BadRequest(message) if message.contains("1001 repositories"))
        );
    }

    #[test]
    fn cursor_target_cap_applies_before_deduplication() {
        let repeated = SearchTarget::new(
            1,
            RepoQualifier::new("repo".to_string()),
            ShardRevision::new("revision".to_string()),
        );
        let cursor = cursor::encode(&SearchCursor {
            query: "query".to_string(),
            kinds: None,
            mode: "lexical".to_string(),
            targets: vec![repeated; MAX_SEARCH_TARGETS + 1],
            offset: 0,
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let outcome = runtime.block_on(search(
            Arc::new(CountingResolver::new(0)),
            SearchRequest {
                query: None,
                kinds: None,
                repos: None,
                mode: None,
                limit: None,
                cursor: Some(cursor),
            },
        ));
        let error = match outcome {
            Err(error) => error,
            Ok(_) => panic!("the raw untrusted target list must be capped"),
        };
        assert!(
            matches!(error, VerbError::BadRequest(message) if message == "invalid cursor: it names too many repositories")
        );
    }

    #[test]
    fn fanout_never_exceeds_the_named_concurrency_bound() {
        let resolver = Arc::new(CountingResolver::new(MAX_CONCURRENT_SEARCH_FANOUT * 2));
        let targets = (0..resolver.target_count)
            .map(CountingResolver::target)
            .collect();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let result = runtime.block_on(rank_targets(
            resolver.clone(),
            targets,
            "query".to_string(),
            None,
            false,
        ));
        assert!(
            matches!(result, Err(VerbError::Internal(_))),
            "fixture paths intentionally fail after concurrency is measured"
        );
        assert_eq!(
            resolver.peak.load(Ordering::SeqCst),
            MAX_CONCURRENT_SEARCH_FANOUT
        );
        assert_eq!(
            resolver.resolutions.load(Ordering::SeqCst),
            MAX_CONCURRENT_SEARCH_FANOUT,
            "a failing batch must not admit later targets"
        );
        assert_eq!(resolver.active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn snippet_hydration_reuses_the_ranked_index_handle() {
        static TEMP_ID: AtomicUsize = AtomicUsize::new(0);
        let temp = std::env::temp_dir().join(format!(
            "yg-verbs-search-handle-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let packed = yg_shard::build_fts(&[
            yg_shard::SearchDoc {
                node_id: "file:README.md".to_string(),
                kind: yg_shard::NodeKind::File,
                name: Some("README.md".to_string()),
                path: Some("README.md".to_string()),
                content: "this project applies a rate limit".to_string(),
            },
            yg_shard::SearchDoc {
                node_id: "file:GUIDE.md".to_string(),
                kind: yg_shard::NodeKind::File,
                name: Some("GUIDE.md".to_string()),
                path: Some("GUIDE.md".to_string()),
                content: "configure the rate limit".to_string(),
            },
        ])
        .expect("build fixture FTS");
        yg_shard::unpack_fts(&packed, &temp).expect("unpack fixture FTS");

        let resolver = Arc::new(CountingResolver::with_fts_path(temp.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let response = runtime
            .block_on(search(
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
        assert!(response.hits[0].snippet.is_some(), "snippet was hydrated");
        let next = response
            .next
            .as_ref()
            .expect("the engine returns a typed continuation");
        let encoded = cursor::encode(next);
        let decoded: SearchCursor = cursor::decode(&encoded).expect("typed continuation encodes");
        assert_eq!(decoded.query, "rate limit");
        assert_eq!(decoded.mode, "lexical");
        assert_eq!(decoded.offset, 1);
        assert_eq!(decoded.targets.len(), 1);
        assert_eq!(
            resolver.resolutions.load(Ordering::SeqCst),
            1,
            "the target was resolved once; hydration reused its open index"
        );
        std::fs::remove_dir_all(temp).expect("remove fixture FTS");
    }
}
