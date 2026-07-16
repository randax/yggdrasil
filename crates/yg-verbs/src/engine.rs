//! The one entry point Verb consumers call (issue #57): the engine owns
//! the whole Verb contract — cursor decode/agreement/encode, limit
//! validation, Shard resolution, and the blocking-execution rule (graph
//! segments are SQLite; opens and reads happen off the async runtime's
//! threads) — so every transport serves identical Verbs by construction.
//!
//! Transports stay thin: they decode a request, call the engine, and
//! encode the typed result or the sanitized [`VerbError`].

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::cursor::{self, HistoryCursor, NeighborsCursor};
use crate::search::{RepoQualifier, SearchResponse, SearchTarget};
use crate::{
    AddressedResponse, AmbiguousNodeAddress, AmbiguousResolution, DEFAULT_HISTORY_LIMIT,
    DEFAULT_NEIGHBORS_DEPTH, DEFAULT_NEIGHBORS_LIMIT, Direction, GraphEdge, HistoryCommit,
    HistoryOptions, HistoryRequest, MAX_HISTORY_LIMIT, MIN_PAGE_LIMIT, NeighborsOptions,
    NeighborsRequest, NoSuchSymbol, NoSuchSymbolKind, NodeRequest, NodeResponse, NodeView, VerbId,
};

/// The sanitized error every Verb leaves the engine through: a client
/// category plus a client-safe message. `Internal` carries the full
/// error chain for the *transport* to log — its content (database
/// errors, filesystem paths, store addresses) never belongs in a
/// response body.
#[derive(Debug, thiserror::Error)]
pub enum VerbError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("no such symbol")]
    NoSuchSymbol(NoSuchSymbol),
    #[error("{0}")]
    Gone(String),
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(anyhow::Error),
}

/// Why a repo qualifier failed to resolve to a local graph segment.
/// The resolver reports the category; the engine owns the client-facing
/// words, because whether a missing revision is the client's expired
/// cursor or a server fault depends on cursor context only the engine
/// has.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no indexed repository matches the qualifier")]
    UnknownRepo,
    #[error("the repository is registered but not yet indexed")]
    NotIndexed,
    #[error("the revision is no longer in storage")]
    RevisionMissing(#[source] anyhow::Error),
    #[error("the Shard predates the current index schema")]
    SchemaOutdated,
    #[error(transparent)]
    Internal(anyhow::Error),
}

/// A repo's graph segment, resolved and verified on local disk.
pub struct ResolvedShard {
    /// Local path of the verified graph segment file.
    pub path: std::path::PathBuf,
    /// The immutable Shard revision the path holds — what a pagination
    /// cursor pins.
    pub revision: String,
    /// Keeps capacity eviction from unlinking the path before it is opened.
    pub cache_lease: Option<yg_shard::CacheLease>,
}

/// A locally materialized FTS segment with an optional cache lease.
pub struct ResolvedFts {
    pub path: std::path::PathBuf,
    pub cache_lease: Option<yg_shard::CacheLease>,
}

/// Matching graph and FTS segments for one immutable Shard revision.
/// Only fuzzy addressing materializes both; exact ids retain the graph-only
/// resolver path and its existing latency and failure behavior.
pub struct ResolvedFuzzyShard {
    pub graph_path: std::path::PathBuf,
    pub fts_path: std::path::PathBuf,
    pub revision: crate::ShardRevision,
    pub graph_cache_lease: Option<yg_shard::CacheLease>,
    pub fts_cache_lease: Option<yg_shard::CacheLease>,
}

/// How the engine reaches Shards: resolve a repo qualifier — via the
/// pointer, or `pinned` (from a pagination cursor) bypassing it to read
/// that exact immutable revision — to a verified local graph segment.
/// Implemented by the deployment (yg-api resolves through the control
/// plane and the segment cache); tests can resolve to fixture files.
pub trait ShardResolver: Send + Sync {
    fn resolve(
        &self,
        qualifier: &str,
        pinned: Option<String>,
    ) -> impl Future<Output = Result<ResolvedShard, ResolveError>> + Send;

    /// Resolve one explicitly named repo to the revision a fresh search pins.
    fn resolve_search_target(
        &self,
        qualifier: &RepoQualifier,
    ) -> impl Future<Output = Result<SearchTarget, ResolveError>> + Send;

    /// List every current-schema indexed repo for an org-wide search.
    fn indexed_search_targets(
        &self,
    ) -> impl Future<Output = Result<Vec<SearchTarget>, ResolveError>> + Send;

    /// Materialize the FTS segment pinned by a search target.
    fn resolve_fts(
        &self,
        target: &SearchTarget,
    ) -> impl Future<Output = Result<ResolvedFts, ResolveError>> + Send;

    /// Resolve matching graph and FTS segments for a fuzzy address.
    fn resolve_fuzzy(
        &self,
        _qualifier: &RepoQualifier,
        _pinned: Option<crate::ShardRevision>,
    ) -> impl Future<Output = Result<ResolvedFuzzyShard, ResolveError>> + Send {
        async {
            Err(ResolveError::Internal(anyhow::anyhow!(
                "fuzzy resolution is not implemented by this Shard resolver"
            )))
        }
    }
}

/// Map a resolution failure to the client's error. The words are the
/// contract every transport serves; `from_cursor` decides whether a
/// missing or outdated revision is the client's expired cursor (410) or
/// the server's problem (500 / 503).
fn resolve_error(qualifier: &str, from_cursor: bool, e: ResolveError) -> VerbError {
    match e {
        ResolveError::UnknownRepo => {
            VerbError::NotFound(format!("no indexed repository matches {qualifier:?}"))
        }
        ResolveError::NotIndexed => VerbError::NotFound(format!(
            "{qualifier} is registered but not yet indexed; try again shortly"
        )),
        // A pinned revision that storage no longer holds is a cursor
        // outliving its Shard (GC, a forged or mistyped cursor): the
        // client must restart the traversal, the server is fine. A fresh
        // query resolving a current pointer to a missing revision is an
        // unexpected server fault.
        ResolveError::RevisionMissing(e) => {
            if from_cursor {
                VerbError::Gone(
                    "this cursor's Shard revision is no longer available; \
                     restart the traversal without a cursor"
                        .to_string(),
                )
            } else {
                VerbError::Internal(e)
            }
        }
        // A revision published under an older index schema: a cursor
        // that outlived a deploy has simply expired; a current pointer
        // is already queued for re-indexing (worker boot requeues every
        // outdated Shard), so the client should retry, not despair.
        ResolveError::SchemaOutdated => {
            if from_cursor {
                VerbError::Gone(
                    "this cursor's Shard revision predates the current index schema; \
                     restart the traversal without a cursor"
                        .to_string(),
                )
            } else {
                VerbError::Unavailable(
                    "this repo's Shard predates the current index schema and is being \
                     re-indexed; try again shortly"
                        .to_string(),
                )
            }
        }
        ResolveError::Internal(e) => VerbError::Internal(e),
    }
}

fn parse_node_address(
    raw_id: &str,
    repo: Option<RepoQualifier>,
    path: Option<crate::SearchPath>,
) -> Result<crate::fuzzy::NodeAddress, VerbError> {
    crate::fuzzy::parse_address(raw_id, repo, path).map_err(VerbError::BadRequest)
}

/// The `neighbors` answer as every transport serves it: one page plus
/// the opaque cursor.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeighborsResponse {
    pub nodes: Vec<NodeView>,
    pub edges: Vec<GraphEdge>,
    pub next_cursor: Option<String>,
}

/// One commit as every transport serves it: the Verb's commit plus a
/// display-ready date — the engine owns rendering (like search
/// snippets), so clients print it verbatim instead of each re-deriving
/// a format from `committed_at`.
#[derive(Debug)]
pub struct HistoryCommitView {
    pub commit: HistoryCommit,
    /// `committed_at` as RFC3339 UTC (`2024-01-01T00:00:00Z`).
    pub date: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryCommitViewWire {
    commit: String,
    sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    committed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<crate::HistoryAuthor>,
    date: String,
}

impl From<&HistoryCommitView> for HistoryCommitViewWire {
    fn from(view: &HistoryCommitView) -> Self {
        let HistoryCommitView {
            commit:
                HistoryCommit {
                    commit,
                    sha,
                    subject,
                    committed_at,
                    author,
                },
            date,
        } = view;
        Self {
            commit: commit.clone(),
            sha: sha.clone(),
            subject: subject.clone(),
            committed_at: *committed_at,
            author: author.as_ref().map(|author| {
                let crate::HistoryAuthor { id, name, email } = author;
                crate::HistoryAuthor {
                    id: id.clone(),
                    name: name.clone(),
                    email: email.clone(),
                }
            }),
            date: date.clone(),
        }
    }
}

impl From<HistoryCommitViewWire> for HistoryCommitView {
    fn from(wire: HistoryCommitViewWire) -> Self {
        let HistoryCommitViewWire {
            commit,
            sha,
            subject,
            committed_at,
            author,
            date,
        } = wire;
        Self {
            commit: HistoryCommit {
                commit,
                sha,
                subject,
                committed_at,
                author,
            },
            date,
        }
    }
}

impl Serialize for HistoryCommitView {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        HistoryCommitViewWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HistoryCommitView {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        HistoryCommitViewWire::deserialize(deserializer).map(Into::into)
    }
}

/// The `history` answer as every transport serves it: one page of
/// commits plus the opaque cursor.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryResponse {
    pub commits: Vec<HistoryCommitView>,
    pub next_cursor: Option<String>,
}

/// Render a unix-seconds committer date as RFC3339 UTC for display.
/// `None` for a timestamp outside chrono's representable range — a
/// corrupted Shard row, which the caller surfaces as an internal fault
/// rather than putting an empty or invented date on the wire.
fn render_date(committed_at: i64) -> Option<String> {
    use chrono::{SecondsFormat, TimeZone, Utc};
    Utc.timestamp_opt(committed_at, 0)
        .single()
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Parse a `history` `since` filter into unix seconds: an RFC3339
/// instant first, then a plain `YYYY-MM-DD` taken at midnight UTC. The
/// error is client-facing.
fn parse_since(raw: &str) -> Result<i64, String> {
    use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
    if let Ok(instant) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(instant.timestamp());
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(Utc
            .from_utc_datetime(&date.and_time(NaiveTime::MIN))
            .timestamp());
    }
    Err(format!(
        "invalid since {raw:?}: expected an RFC3339 timestamp \
         (2024-01-01T00:00:00Z) or a date (2024-01-01)"
    ))
}

/// The Verb engine: [`ShardResolver`] in, identical Verbs out, for
/// every transport (RFC 0001 §7 mandates one behavior across REST, MCP,
/// and CLI — this is the one implementation that makes it structural).
pub struct Engine<R> {
    resolver: std::sync::Arc<R>,
    metrics: crate::Metrics,
}

impl<R: ShardResolver + 'static> Engine<R> {
    pub fn new(resolver: R) -> Self {
        Self::with_metrics(resolver, crate::Metrics::unregistered())
    }

    /// Build an engine using the supplied Verb latency collectors.
    pub fn with_metrics(resolver: R, metrics: crate::Metrics) -> Self {
        Self {
            resolver: std::sync::Arc::new(resolver),
            metrics,
        }
    }

    /// The metrics handle shared with API-owned Verbs such as `search`.
    pub fn metrics(&self) -> &crate::Metrics {
        &self.metrics
    }

    /// The `node` Verb, end to end: parse, resolve, read.
    pub async fn node(
        &self,
        req: NodeRequest,
    ) -> Result<AddressedResponse<NodeResponse>, VerbError> {
        let _timer = self.metrics.timer(crate::Verb::Node);
        let resolved = self
            .resolve_node_address(req.id, req.repo, req.path, None)
            .await?;
        let ResolvedAddress::Exact { shard, id } = resolved else {
            return Ok(resolved.into_ambiguous());
        };
        run_verb(shard.path, shard.cache_lease, id, crate::node)
            .await
            .map(AddressedResponse::Resolved)
    }

    /// The `search` Verb, end to end: cursor policy, target resolution,
    /// bounded FTS fan-out, deterministic merge, and snippet hydration.
    pub async fn search(&self, req: crate::SearchRequest) -> Result<SearchResponse, VerbError> {
        crate::search::search(self.resolver.clone(), req).await
    }

    /// The `neighbors` Verb, end to end: cursor decode and agreement,
    /// validation, resolve (pinned by the cursor where present), read,
    /// cursor encode.
    pub async fn neighbors(
        &self,
        req: NeighborsRequest,
    ) -> Result<AddressedResponse<NeighborsResponse>, VerbError> {
        let _timer = self.metrics.timer(crate::Verb::Neighbors);
        let cursor = req
            .cursor
            .as_deref()
            .map(cursor::decode::<NeighborsCursor>)
            .transpose()
            .map_err(VerbError::BadRequest)?;
        if let Some(cursor) = &cursor {
            cursor
                .agrees_with(&req.shape)
                .map_err(VerbError::BadRequest)?;
        }
        // The shape in force: the cursor's where present (it remembers
        // the original request, and agrees_with just ruled out
        // contradiction), the request's on a fresh traversal. Only the
        // page size may vary mid-walk.
        let shape = match cursor.as_ref() {
            Some(cursor) => cursor.shape.clone(),
            None => req.shape,
        };
        let direction = shape
            .direction
            .as_deref()
            .map(Direction::parse)
            .transpose()
            .map_err(VerbError::BadRequest)?
            .unwrap_or_default();
        let options = NeighborsOptions {
            direction,
            edge_kinds: shape.edge_kinds.clone(),
            depth: shape.depth.unwrap_or(DEFAULT_NEIGHBORS_DEPTH),
            limit: req.limit.unwrap_or(DEFAULT_NEIGHBORS_LIMIT as u32) as usize,
            after: cursor.as_ref().map(|c| (c.after_depth, c.after.clone())),
        };
        options.validate().map_err(VerbError::BadRequest)?;

        // A cursor pins the revision its traversal started on; fresh
        // queries resolve the current pointer.
        let pinned = cursor.as_ref().map(|c| c.rev.clone());
        let resolved = self
            .resolve_node_address(
                shape.id.clone(),
                shape.repo.clone(),
                shape.path.clone(),
                pinned,
            )
            .await?;
        let ResolvedAddress::Exact { shard, id } = resolved else {
            return Ok(resolved.into_ambiguous());
        };

        let verb_options = options.clone();
        let page = run_verb(shard.path, shard.cache_lease, id, move |conn, id| {
            crate::neighbors(conn, id, &verb_options)
        })
        .await?;

        let next_cursor = page.next.as_ref().map(|(after_depth, after)| {
            cursor::encode(&NeighborsCursor {
                rev: shard.revision.clone(),
                shape: shape.clone(),
                after_depth: *after_depth,
                after: after.clone(),
            })
        });
        Ok(AddressedResponse::Resolved(NeighborsResponse {
            nodes: page.nodes,
            edges: page.edges,
            next_cursor,
        }))
    }

    /// The `history` Verb, end to end: cursor decode and agreement,
    /// `since` normalization, validation, resolve (pinned by the cursor
    /// where present), read, cursor encode, date rendering.
    pub async fn history(
        &self,
        req: HistoryRequest,
    ) -> Result<AddressedResponse<HistoryResponse>, VerbError> {
        let _timer = self.metrics.timer(crate::Verb::History);
        let cursor = req
            .cursor
            .as_deref()
            .map(cursor::decode::<HistoryCursor>)
            .transpose()
            .map_err(VerbError::BadRequest)?;
        let req_since = req
            .since
            .as_deref()
            .map(parse_since)
            .transpose()
            .map_err(VerbError::BadRequest)?;
        if let Some(cursor) = &cursor {
            cursor
                .agrees_with(&req.id, req.repo.as_ref(), req.path.as_ref(), req_since)
                .map_err(VerbError::BadRequest)?;
        }
        // The id + since in force: the cursor's where resuming (it
        // remembers the original, and agrees_with ruled out
        // contradiction), the request's on a fresh history.
        let (id_str, repo, path, since) = match &cursor {
            Some(cursor) => (
                cursor.id.clone(),
                cursor.repo.clone(),
                cursor.path.clone(),
                cursor.since,
            ),
            None => (
                req.id.clone(),
                req.repo.clone(),
                req.path.clone(),
                req_since,
            ),
        };
        let limit = req.limit.map_or(DEFAULT_HISTORY_LIMIT, |l| l as usize);
        if !(MIN_PAGE_LIMIT..=MAX_HISTORY_LIMIT).contains(&limit) {
            return Err(VerbError::BadRequest(format!(
                "limit must be between {MIN_PAGE_LIMIT} and {MAX_HISTORY_LIMIT}, got {limit}"
            )));
        }
        // A cursor pins the revision its history started on; fresh
        // queries resolve the current pointer.
        let pinned = cursor.as_ref().map(|c| c.rev.clone());
        let resolved = self
            .resolve_node_address(id_str.clone(), repo.clone(), path.clone(), pinned)
            .await?;
        let ResolvedAddress::Exact { shard, id } = resolved else {
            return Ok(resolved.into_ambiguous());
        };
        let options = HistoryOptions {
            limit,
            since,
            after: cursor
                .as_ref()
                .map(|c| (c.after_committed_at, c.after_sha.clone())),
        };
        let page = run_verb(shard.path, shard.cache_lease, id, move |conn, id| {
            crate::history(conn, id, &options)
        })
        .await?;
        let next_cursor = page.next.as_ref().map(|(at, sha)| {
            cursor::encode(&HistoryCursor {
                rev: shard.revision.clone(),
                id: id_str.clone(),
                repo: repo.clone(),
                path: path.clone(),
                since,
                after_committed_at: *at,
                after_sha: sha.clone(),
            })
        });
        let commits = page
            .commits
            .into_iter()
            .map(|commit| {
                let date = render_date(commit.committed_at).ok_or_else(|| {
                    VerbError::Internal(anyhow::anyhow!(
                        "commit {} carries committer date {} outside the renderable range; \
                         refusing to serve a corrupted history",
                        commit.sha,
                        commit.committed_at
                    ))
                })?;
                Ok(HistoryCommitView { date, commit })
            })
            .collect::<Result<Vec<_>, VerbError>>()?;
        Ok(AddressedResponse::Resolved(HistoryResponse {
            commits,
            next_cursor,
        }))
    }

    async fn resolve_node_address(
        &self,
        raw_id: String,
        repo: Option<RepoQualifier>,
        path: Option<crate::SearchPath>,
        pinned: Option<String>,
    ) -> Result<ResolvedAddress, VerbError> {
        let address = parse_node_address(&raw_id, repo, path)?;
        let from_cursor = pinned.is_some();
        match address {
            crate::fuzzy::NodeAddress::Exact(id) => {
                let shard = self
                    .resolver
                    .resolve(&id.repo, pinned)
                    .await
                    .map_err(|error| resolve_error(&id.repo, from_cursor, error))?;
                Ok(ResolvedAddress::Exact { shard, id })
            }
            crate::fuzzy::NodeAddress::Fuzzy(address) => {
                let qualifier = address.repo.clone();
                let resolved = self
                    .resolver
                    .resolve_fuzzy(&qualifier, pinned.map(crate::ShardRevision::new))
                    .await
                    .map_err(|error| resolve_error(qualifier.as_str(), from_cursor, error))?;
                let name = address.name.as_str().to_string();
                let path = address.path.as_ref().map(|path| path.as_str().to_string());
                let fts_path = resolved.fts_path;
                let fts_cache_lease = resolved.fts_cache_lease;
                let symbols = tokio::task::spawn_blocking(move || {
                    let index = yg_shard::open_fts(&fts_path)?;
                    let symbols = yg_shard::symbols_named(&index, &name, path.as_deref());
                    drop(fts_cache_lease);
                    symbols
                })
                .await
                .map_err(|error| {
                    VerbError::Internal(
                        anyhow::Error::new(error).context("fuzzy resolution task panicked"),
                    )
                })?;
                let symbols = match symbols {
                    Ok(symbols) => symbols,
                    Err(error)
                        if error
                            .downcast_ref::<yg_shard::UnaddressableSymbolName>()
                            .is_some() =>
                    {
                        return Err(VerbError::NoSuchSymbol(NoSuchSymbol {
                            kind: NoSuchSymbolKind::UnaddressableSymbol,
                            address,
                        }));
                    }
                    Err(error) => return Err(VerbError::Internal(error)),
                };
                let total_matches = symbols.len();
                let mut candidates = crate::fuzzy::rank_candidates(&address, symbols);
                match total_matches {
                    0 => Err(VerbError::NoSuchSymbol(NoSuchSymbol {
                        kind: NoSuchSymbolKind::NoSuchSymbol,
                        address,
                    })),
                    1 => Ok(ResolvedAddress::Exact {
                        shard: ResolvedShard {
                            path: resolved.graph_path,
                            revision: resolved.revision.as_str().to_string(),
                            cache_lease: resolved.graph_cache_lease,
                        },
                        id: candidates.pop().expect("one candidate").id,
                    }),
                    _ => Ok(ResolvedAddress::Ambiguous(AmbiguousNodeAddress {
                        resolution: AmbiguousResolution::Ambiguous,
                        address,
                        total_matches,
                        candidates,
                    })),
                }
            }
        }
    }
}

enum ResolvedAddress {
    Exact { shard: ResolvedShard, id: VerbId },
    Ambiguous(AmbiguousNodeAddress),
}

impl ResolvedAddress {
    fn into_ambiguous<T>(self) -> AddressedResponse<T> {
        match self {
            Self::Ambiguous(ambiguous) => AddressedResponse::Ambiguous(ambiguous),
            Self::Exact { .. } => unreachable!("the caller matched the exact variant"),
        }
    }
}

/// The shared back half of every node-addressed Verb: open the resolved
/// graph segment and run the (blocking, SQLite-bound) verb off the
/// runtime threads — the open does filesystem syscalls, so it belongs
/// in the closure too. The verb's `None` is the client's 404.
async fn run_verb<T, F>(
    path: std::path::PathBuf,
    cache_lease: Option<yg_shard::CacheLease>,
    id: VerbId,
    verb: F,
) -> Result<T, VerbError>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection, &VerbId) -> anyhow::Result<Option<T>> + Send + 'static,
{
    let external = id.external();
    let outcome = tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .context("opening the cached graph segment")?;
        let result = verb(&conn, &id);
        drop(cache_lease);
        result
    })
    .await;
    match outcome {
        Ok(Ok(Some(response))) => Ok(response),
        // "this", not "the current": a pagination cursor may have
        // pinned an older revision than the pointer's.
        Ok(Ok(None)) => Err(VerbError::NotFound(format!(
            "no node {external} in this repo's Shard"
        ))),
        Ok(Err(e)) => Err(VerbError::Internal(e)),
        Err(e) => Err(VerbError::Internal(
            anyhow::Error::new(e).context("verb task panicked"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_parses_rfc3339_and_dates() {
        assert_eq!(parse_since("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_since("1970-01-02"), Ok(86_400));
        assert_eq!(
            parse_since("1970-01-01T02:00:00+02:00"),
            Ok(0),
            "offsets normalize to the same instant"
        );
        let err = parse_since("last tuesday").unwrap_err();
        assert!(err.contains("last tuesday") && err.contains("RFC3339"));
    }

    #[test]
    fn dates_render_rfc3339_utc() {
        assert_eq!(render_date(0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(
            render_date(1_704_067_200).as_deref(),
            Some("2024-01-01T00:00:00Z")
        );
        // A timestamp chrono cannot represent is a corrupted Shard row:
        // no date, never an empty string on the wire.
        assert_eq!(render_date(i64::MAX), None);
    }

    /// The resolver's categories map to the client statuses the Verb
    /// contract promises: cursor-context decides expired-cursor (Gone)
    /// versus server fault (Internal / Unavailable).
    #[test]
    fn resolve_errors_map_by_cursor_context() {
        let missing = || ResolveError::RevisionMissing(anyhow::anyhow!("gone from storage"));
        assert!(matches!(
            resolve_error("q", true, missing()),
            VerbError::Gone(_)
        ));
        assert!(matches!(
            resolve_error("q", false, missing()),
            VerbError::Internal(_)
        ));
        assert!(matches!(
            resolve_error("q", true, ResolveError::SchemaOutdated),
            VerbError::Gone(_)
        ));
        assert!(matches!(
            resolve_error("q", false, ResolveError::SchemaOutdated),
            VerbError::Unavailable(_)
        ));
        assert!(matches!(
            resolve_error("q", false, ResolveError::UnknownRepo),
            VerbError::NotFound(_)
        ));
        assert!(matches!(
            resolve_error("q", false, ResolveError::NotIndexed),
            VerbError::NotFound(_)
        ));
    }

    #[test]
    fn overlong_fuzzy_names_are_typed_bad_requests() {
        let outcome = parse_node_address(
            &"a".repeat(crate::fuzzy::MAX_ADDRESS_NAME_BYTES + 1),
            Some(RepoQualifier::new("github.com/acme/widgets".to_string())),
            None,
        );

        assert!(matches!(
            outcome,
            Err(VerbError::BadRequest(message))
                if message == format!(
                    "fuzzy symbol name is {} bytes; the limit is {}",
                    crate::fuzzy::MAX_ADDRESS_NAME_BYTES + 1,
                    crate::fuzzy::MAX_ADDRESS_NAME_BYTES
                )
        ));
    }
}
