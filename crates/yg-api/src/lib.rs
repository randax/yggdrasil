//! REST + MCP server.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use object_store::ObjectStore;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use yg_control::ControlPlane;
// Server config embeds the object-store half owned by yg-shard; clients
// of this crate keep addressing it as `yg_api::ObjectStoreConfig`.
pub use yg_shard::{ObjectStoreConfig, probe_object_store};
use yg_sync::RepoLocator;

pub struct ServerConfig {
    pub listen: SocketAddr,
    pub database_url: String,
    pub object_store: ObjectStoreConfig,
    pub bootstrap_token: String,
    /// Local tier for Shard segments (RFC 0001 §6): warm Verb queries
    /// read from here instead of object storage.
    pub shard_cache: std::path::PathBuf,
}

impl ServerConfig {
    /// Build from `YG_*` environment variables. Everything defaults to the
    /// in-repo dev compose stack except the bootstrap Admin token, which
    /// has no safe default.
    pub fn from_env() -> anyhow::Result<Self> {
        fn var_or(key: &str, default: &str) -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_string())
        }
        // Trimmed before storing: env files commonly leak whitespace, and
        // HTTP strips it from header values, so a padded token could never
        // be presented by any client.
        let bootstrap_token = std::env::var("YG_BOOTSTRAP_TOKEN")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .context(
                "YG_BOOTSTRAP_TOKEN must be set to a non-empty token; \
                 the server refuses to boot without an Admin token",
            )?;
        Ok(Self {
            listen: var_or("YG_LISTEN", "127.0.0.1:7311")
                .parse()
                .context("parsing YG_LISTEN as host:port")?,
            database_url: var_or("YG_DATABASE_URL", yg_control::DEFAULT_DATABASE_URL),
            object_store: ObjectStoreConfig::from_env(),
            bootstrap_token,
            shard_cache: var_or("YG_SHARD_CACHE", "./data/shard-cache").into(),
        })
    }
}

/// A booted Index Server, listening until dropped or the process exits.
pub struct RunningServer {
    local_addr: SocketAddr,
    handle: JoinHandle<std::io::Result<()>>,
}

impl RunningServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run until the server task ends (it normally never does); a serve
    /// error surfaces here instead of being silently logged.
    pub async fn wait(self) -> anyhow::Result<()> {
        self.handle
            .await
            .context("server task panicked")?
            .context("server exited with an error")
    }
}

struct AppState {
    control: ControlPlane,
    store: std::sync::Arc<dyn ObjectStore>,
    shards: yg_shard::ShardCache,
    bootstrap_token: String,
    started: std::time::Instant,
}

/// Boot the Index Server: connect to the control plane, verify object
/// storage, and start serving.
pub async fn serve(config: ServerConfig) -> anyhow::Result<RunningServer> {
    let control = ControlPlane::connect_and_migrate(&config.database_url).await?;

    let store = config.object_store.connect()?;
    probe_object_store(store.as_ref())
        .await
        .context("object storage unreachable at boot")?;

    let state = Arc::new(AppState {
        control,
        shards: yg_shard::ShardCache::new(store.clone(), config.shard_cache),
        store,
        bootstrap_token: config.bootstrap_token,
        started: std::time::Instant::now(),
    });
    // The auth layer wraps the whole app — including fallbacks, so even
    // nonexistent paths answer 401 to unauthenticated callers.
    let app = Router::new()
        .route("/healthz", get(healthz))
        .nest(
            "/v1",
            Router::new()
                .route("/status", get(status))
                .route("/verbs/node", post(verb_node))
                .route("/verbs/neighbors", post(verb_neighbors))
                .route("/verbs/search", post(verb_search))
                .route("/admin/repos", post(admin_repo_add))
                .route("/admin/status", get(admin_status)),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer_token,
        ))
        .with_state(state);

    let listener = TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    let local_addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let result = axum::serve(listener, app).await;
        if let Err(e) = &result {
            tracing::error!("server exited: {e}");
        }
        result
    });

    Ok(RunningServer { local_addr, handle })
}

/// Every route — existing or not — requires the bootstrap Admin token;
/// only the health endpoint is reachable without one.
async fn require_bearer_token(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    use subtle::ConstantTimeEq;

    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }

    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        // RFC 9110: the scheme is case-insensitive.
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        // RFC 9110 allows 1*SP between scheme and credentials.
        .is_some_and(|(_, presented)| {
            presented
                .trim_start_matches(' ')
                .as_bytes()
                .ct_eq(state.bootstrap_token.as_bytes())
                .into()
        });
    if authorized {
        next.run(req).await
    } else {
        error_json(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
    }
}

/// The one shape every error leaves this server in: `{"error": "…"}`.
fn error_json(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({"error": message.into()}))).into_response()
}

#[derive(Deserialize)]
struct NodeRequest {
    id: String,
}

/// `POST /v1/verbs/node` (RFC 0001 §7): full node + edge summary.
async fn verb_node(State(state): State<Arc<AppState>>, Json(req): Json<NodeRequest>) -> Response {
    let id = match yg_verbs::VerbId::parse(&req.id) {
        Ok(id) => id,
        Err(reason) => return error_json(StatusCode::BAD_REQUEST, reason),
    };
    let (path, _) = match resolve_shard(&state, &id.repo, None).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    match run_verb(path, id, yg_verbs::node).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct NeighborsRequest {
    #[serde(flatten)]
    shape: TraversalShape,
    /// Page size in nodes: 1 to 1000, default 100. Deliberately not
    /// part of the shape: pages of one traversal may vary in size.
    limit: Option<u32>,
    /// Resume an earlier traversal where its last page ended.
    cursor: Option<String>,
}

/// The traversal-defining half of a `neighbors` request: origin and
/// filters, exactly as the client spelled them. One definition, two
/// homes — the request and the cursor — so "the cursor remembers what
/// was asked" is a move, not a field-by-field copy that can drift.
#[derive(Serialize, Deserialize, Clone)]
struct TraversalShape {
    id: String,
    /// `"in"` | `"out"` | `"both"` (the default). A plain string so a
    /// typo gets this server's error envelope, not a serde rejection.
    direction: Option<String>,
    /// Only follow edges of these kinds; omitted follows every kind.
    edge_kinds: Option<Vec<String>>,
    /// Hops to traverse: 1 (default) to 3 (RFC 0001 §7).
    depth: Option<u32>,
}

/// What a `next_cursor` opaquely carries: the traversal position, the
/// Shard revision it was read from, and the request shape it belongs
/// to. Later pages stay on the pinned revision — Shards are immutable,
/// so a paginated walk sees one consistent graph even across a pointer
/// swap; only a *fresh* query picks up the new Shard. The request shape
/// rides along because the page contract ("pages of one traversal union
/// to the full induced subgraph") only holds when every page is
/// computed with identical origin and filters: a replay that
/// contradicts its cursor is rejected, never silently served from a
/// different traversal.
#[derive(Serialize, Deserialize)]
struct NeighborsCursor {
    rev: String,
    #[serde(flatten)]
    shape: TraversalShape,
    after_depth: u32,
    after: String,
}

impl NeighborsCursor {
    fn encode(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).expect("a cursor serializes"))
    }

    fn decode(cursor: &str) -> Result<Self, String> {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| {
                "invalid cursor: pass back next_cursor from a previous response, unmodified"
                    .to_string()
            })
    }

    /// The cursor remembers what the first page was asked; a follow-up
    /// may repeat those fields (in any equivalent spelling) or omit
    /// them, nothing else. Spellings are compared normalized: an
    /// omitted direction and an explicit `"both"` mean the same
    /// traversal, and edge-kind order carries no meaning.
    fn agrees_with(&self, req: &TraversalShape) -> Result<(), String> {
        fn direction(spelled: &Option<String>) -> Result<yg_verbs::Direction, String> {
            spelled.as_deref().map_or(
                Ok(yg_verbs::Direction::default()),
                yg_verbs::Direction::parse,
            )
        }
        fn kind_set(kinds: &Option<Vec<String>>) -> Option<Vec<String>> {
            kinds.clone().map(|mut kinds| {
                kinds.sort_unstable();
                kinds.dedup();
                kinds
            })
        }
        let contradicts = req.id != self.shape.id
            || (req.direction.is_some()
                && direction(&req.direction)? != direction(&self.shape.direction)?)
            || (req.edge_kinds.is_some()
                && kind_set(&req.edge_kinds) != kind_set(&self.shape.edge_kinds))
            || req
                .depth
                .is_some_and(|d| d != self.shape.depth.unwrap_or(1));
        if contradicts {
            return Err(
                "this cursor belongs to a different request (id, direction, edge_kinds, \
                 and depth must match the page it came from); start a fresh traversal \
                 or pass the cursor with the original parameters"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// `POST /v1/verbs/neighbors` (RFC 0001 §7): one page of the adjacent
/// subgraph.
async fn verb_neighbors(
    State(state): State<Arc<AppState>>,
    Json(req): Json<NeighborsRequest>,
) -> Response {
    let cursor = match req.cursor.as_deref().map(NeighborsCursor::decode) {
        Some(Ok(cursor)) => Some(cursor),
        Some(Err(reason)) => return error_json(StatusCode::BAD_REQUEST, reason),
        None => None,
    };
    if let Some(cursor) = &cursor
        && let Err(reason) = cursor.agrees_with(&req.shape)
    {
        return error_json(StatusCode::BAD_REQUEST, reason);
    }
    // The shape in force: the cursor's where present (it remembers the
    // original request, and agrees_with just ruled out contradiction),
    // the request's on a fresh traversal. Only the page size may vary
    // mid-walk.
    let shape = match cursor.as_ref() {
        Some(cursor) => cursor.shape.clone(),
        None => req.shape,
    };
    let direction = match shape.direction.as_deref().map(yg_verbs::Direction::parse) {
        Some(Ok(direction)) => direction,
        Some(Err(reason)) => return error_json(StatusCode::BAD_REQUEST, reason),
        None => yg_verbs::Direction::default(),
    };
    let options = yg_verbs::NeighborsOptions {
        direction,
        edge_kinds: shape.edge_kinds.clone(),
        depth: shape.depth.unwrap_or(1),
        limit: req.limit.unwrap_or(100) as usize,
        after: cursor.as_ref().map(|c| (c.after_depth, c.after.clone())),
    };
    if let Err(reason) = options.validate() {
        return error_json(StatusCode::BAD_REQUEST, reason);
    }

    let id = match yg_verbs::VerbId::parse(&shape.id) {
        Ok(id) => id,
        Err(reason) => return error_json(StatusCode::BAD_REQUEST, reason),
    };
    // A cursor pins the revision its traversal started on; fresh
    // queries resolve the current pointer.
    let pinned = cursor.as_ref().map(|c| c.rev.clone());
    let (path, revision) = match resolve_shard(&state, &id.repo, pinned).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };

    let verb_options = options.clone();
    let page = match run_verb(path, id, move |conn, id| {
        yg_verbs::neighbors(conn, id, &verb_options)
    })
    .await
    {
        Ok(page) => page,
        Err(response) => return response,
    };

    let next_cursor = page.next.as_ref().map(|(after_depth, after)| {
        NeighborsCursor {
            rev: revision.clone(),
            shape: shape.clone(),
            after_depth: *after_depth,
            after: after.clone(),
        }
        .encode()
    });
    Json(NeighborsResponse {
        nodes: page.nodes,
        edges: page.edges,
        next_cursor,
    })
    .into_response()
}

/// The `neighbors` wire response: one page plus the opaque cursor.
#[derive(Serialize)]
struct NeighborsResponse {
    nodes: Vec<yg_verbs::NodeView>,
    edges: Vec<yg_verbs::GraphEdge>,
    next_cursor: Option<String>,
}

/// Default and maximum page size for `search`, plus the deepest a cursor
/// may page. The window also bounds the fan-out: each repo is asked for a
/// constant top-[`MAX_SEARCH_WINDOW`] hits (the deepest reachable page),
/// which both keeps the merged ranking stable across pages and caps the
/// work an unbounded offset could demand.
const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 100;
const MAX_SEARCH_WINDOW: usize = 1000;

/// The most repositories one search may fan out over — the width bound a
/// cursor's (unsigned, hence untrusted) target list is held to, so a
/// tampered cursor can't amplify one request into unbounded segment opens.
/// Generous for M0; org-wide fan-out beyond this is the tiering work
/// deferred to M3 (RFC 0001 §11 Q5).
const MAX_SEARCH_TARGETS: usize = 1000;

#[derive(Deserialize)]
struct SearchRequest {
    /// The query — required on a fresh search; a resume carries it in the
    /// cursor instead (and a query passed alongside a cursor must match).
    query: Option<String>,
    /// Restrict hits to these node kinds (`Symbol`, `File`); omitted
    /// searches every kind.
    kinds: Option<Vec<String>>,
    /// Restrict the fan-out to these repo qualifiers; omitted searches
    /// every indexed repo (RFC 0001 §7).
    repos: Option<Vec<String>>,
    /// `lexical` (the only M0 mode, and the default); `semantic`/`hybrid`
    /// arrive with embeddings (M3).
    mode: Option<String>,
    /// Page size in hits: 1 to [`MAX_SEARCH_LIMIT`], default
    /// [`DEFAULT_SEARCH_LIMIT`]. May vary between pages of one search.
    limit: Option<u32>,
    /// Resume an earlier search where its last page ended.
    cursor: Option<String>,
}

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

impl SearchCursor {
    fn encode(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).expect("a cursor serializes"))
    }

    fn decode(cursor: &str) -> Result<Self, String> {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| {
                "invalid cursor: pass back next_cursor from a previous response, unmodified"
                    .to_string()
            })
    }
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
async fn verb_search(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SearchRequest>,
) -> Response {
    let cursor = match req.cursor.as_deref().map(SearchCursor::decode) {
        Some(Ok(cursor)) => Some(cursor),
        Some(Err(reason)) => return error_json(StatusCode::BAD_REQUEST, reason),
        None => None,
    };

    // Page size may vary between pages; everything else is fixed at the
    // first page and carried by the cursor.
    let limit = req.limit.map_or(DEFAULT_SEARCH_LIMIT, |l| l as usize);
    if !(1..=MAX_SEARCH_LIMIT).contains(&limit) {
        return error_json(
            StatusCode::BAD_REQUEST,
            format!("limit must be between 1 and {MAX_SEARCH_LIMIT}, got {limit}"),
        );
    }

    // The query state in force: the cursor's where present, the request's
    // on a fresh search.
    let (query, kind_strings, mode, targets, offset) = match cursor {
        Some(cursor) => {
            // A query may be re-sent alongside the cursor (compared trimmed,
            // since a fresh search stores the trimmed form) but must match.
            if let Some(q) = req.query.as_deref()
                && q.trim() != cursor.query
            {
                return error_json(
                    StatusCode::BAD_REQUEST,
                    "this cursor belongs to a different query; start a fresh search or \
                     pass the cursor without a query"
                        .to_string(),
                );
            }
            // The cursor is unsigned, so its target list is untrusted: a
            // tampered cursor could repeat or pad it to fan a single
            // request out across an unbounded number of segment opens. Cap
            // and dedup it, mirroring the fresh path's `resolve_search_targets`.
            if cursor.targets.len() > MAX_SEARCH_TARGETS {
                return error_json(
                    StatusCode::BAD_REQUEST,
                    "invalid cursor: it names too many repositories".to_string(),
                );
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
                    return error_json(
                        StatusCode::BAD_REQUEST,
                        "search needs a non-empty query".to_string(),
                    );
                }
            };
            let mode = req.mode.unwrap_or_else(|| "lexical".to_string());
            let targets = match resolve_search_targets(&state, req.repos.as_deref()).await {
                Ok(targets) => targets,
                Err(response) => return response,
            };
            (query, req.kinds, mode, targets, 0)
        }
    };

    // The mode gate runs on the in-force value, so a forged or stale cursor
    // can't smuggle an unsupported mode past the fresh-search check.
    if mode != "lexical" {
        return error_json(
            StatusCode::BAD_REQUEST,
            format!(
                "search mode {mode:?} is not available; only \"lexical\" is supported \
                 (semantic and hybrid arrive with embeddings)"
            ),
        );
    }

    // A cursor's offset is client-supplied (the cursor is unsigned), so
    // bound it before any arithmetic: past the window there is nothing to
    // serve, and an unbounded offset would overflow `offset + …` below.
    if offset >= MAX_SEARCH_WINDOW {
        return error_json(
            StatusCode::BAD_REQUEST,
            format!("cannot page beyond {MAX_SEARCH_WINDOW} results"),
        );
    }
    // Clamp this page to the window: a requested limit that would run past
    // it serves a final short page rather than stranding reachable hits
    // behind a cursor the next request would reject.
    let page_limit = clamped_page_limit(offset, limit);

    // Validate (and parse) the kind filter against the node-kind
    // vocabulary, so a typo errors instead of silently matching nothing.
    let kinds = match parse_search_kinds(kind_strings.as_deref()) {
        Ok(kinds) => kinds,
        Err(reason) => return error_json(StatusCode::BAD_REQUEST, reason),
    };

    // No indexed repos at all (a fresh org-wide search before anything is
    // indexed): an empty result, not an error.
    if targets.is_empty() {
        return Json(SearchResponse {
            hits: vec![],
            next_cursor: None,
        })
        .into_response();
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
        let dir = match state
            .shards
            .fts_path(target.repo_id, &target.revision)
            .await
        {
            Ok(dir) => dir,
            Err(e) => return map_search_shard_error(e, from_cursor),
        };
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
            Ok(Err(e)) => return map_search_query_error(e),
            Err(e) => {
                return error_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("search task panicked: {e}"),
                );
            }
        }
    }

    let (mut page, has_more) = merge_paginate(all, offset, page_limit);
    if let Err(response) = hydrate_snippets(&state, &targets, &query, from_cursor, &mut page).await
    {
        return response;
    }
    // Offer a next page only while one is both available and reachable
    // (paging stops at the window).
    let next_cursor = (has_more && offset + page_limit < MAX_SEARCH_WINDOW).then(|| {
        SearchCursor {
            query,
            kinds: kind_strings,
            mode,
            targets,
            offset: offset + page_limit,
        }
        .encode()
    });
    Json(SearchResponse {
        hits: page,
        next_cursor,
    })
    .into_response()
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
) -> Result<(), Response> {
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
        let dir = match state
            .shards
            .fts_path(target.repo_id, &target.revision)
            .await
        {
            Ok(dir) => dir,
            Err(e) => return Err(map_search_shard_error(e, from_cursor)),
        };
        let local_ids: Vec<String> = indices.iter().map(|&i| page[i].local_id.clone()).collect();
        let query = query.to_string();
        let snippets = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let index = yg_shard::open_fts(&dir)?;
            yg_shard::snippets_for(&index, &query, &local_ids)
        })
        .await;
        let snippets = match snippets {
            Ok(Ok(snippets)) => snippets,
            Ok(Err(e)) => {
                return Err(error_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("{e:#}"),
                ));
            }
            Err(e) => {
                return Err(error_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("snippet task panicked: {e}"),
                ));
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
/// vocabulary rather than silently matching nothing.
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
) -> Result<Vec<SearchTarget>, Response> {
    match repos {
        Some(repos) => {
            let mut targets = Vec::with_capacity(repos.len());
            let mut seen = std::collections::HashSet::new();
            for qualifier in repos {
                // A repo named twice is one target: otherwise its hits
                // would appear twice in the merged page.
                if !seen.insert(qualifier.as_str()) {
                    continue;
                }
                let target = match state.control.verb_target(qualifier).await {
                    Ok(Some(target)) => target,
                    Ok(None) => {
                        return Err(error_json(
                            StatusCode::NOT_FOUND,
                            format!("no indexed repository matches {qualifier:?}"),
                        ));
                    }
                    Err(e) => {
                        return Err(error_json(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("{e:#}"),
                        ));
                    }
                };
                let Some(revision) = target.revision else {
                    return Err(error_json(
                        StatusCode::NOT_FOUND,
                        format!("{qualifier} is registered but not yet indexed; try again shortly"),
                    ));
                };
                targets.push(SearchTarget {
                    repo_id: target.repo_id,
                    qualifier: qualifier.clone(),
                    revision,
                });
            }
            Ok(targets)
        }
        None => match state.control.indexed_repos().await {
            Ok(repos) => Ok(repos
                .into_iter()
                .map(|r| SearchTarget {
                    repo_id: r.repo_id,
                    qualifier: r.qualifier,
                    revision: r.revision,
                })
                .collect()),
            Err(e) => Err(error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{e:#}"),
            )),
        },
    }
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
/// [`resolve_shard`] does for the node-addressed Verbs.
fn map_search_shard_error(e: anyhow::Error, from_cursor: bool) -> Response {
    if from_cursor && e.downcast_ref::<yg_shard::RevisionMissing>().is_some() {
        return error_json(
            StatusCode::GONE,
            "this cursor's Shard revision is no longer available; restart the search without a cursor",
        );
    }
    if e.downcast_ref::<yg_shard::SchemaOutdated>().is_some() {
        return if from_cursor {
            error_json(
                StatusCode::GONE,
                "this cursor's Shard revision predates the current index schema; \
                 restart the search without a cursor",
            )
        } else {
            error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "a repo's Shard predates the current index schema and is being re-indexed; \
                 try again shortly",
            )
        };
    }
    error_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Map a search-execution error: a query tantivy can't parse is the
/// client's to fix (400); anything else is a 500.
fn map_search_query_error(e: anyhow::Error) -> Response {
    match e.downcast_ref::<yg_shard::QueryMalformed>() {
        Some(malformed) => error_json(StatusCode::BAD_REQUEST, malformed.to_string()),
        None => error_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// The shared back half of every node-addressed Verb: open the resolved
/// graph segment and run the (blocking, SQLite-bound) verb off the
/// runtime threads — the open does filesystem syscalls, so it belongs
/// in the closure too. The verb's `None` is the client's 404; errors
/// come back as ready-to-return responses.
async fn run_verb<T, F>(
    path: std::path::PathBuf,
    id: yg_verbs::VerbId,
    verb: F,
) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection, &yg_verbs::VerbId) -> anyhow::Result<Option<T>>
        + Send
        + 'static,
{
    let external = id.external();
    let outcome = tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .context("opening the cached graph segment")?;
        verb(&conn, &id)
    })
    .await;
    match outcome {
        Ok(Ok(Some(response))) => Ok(response),
        // "this", not "the current": a pagination cursor may have
        // pinned an older revision than the pointer's.
        Ok(Ok(None)) => Err(error_json(
            StatusCode::NOT_FOUND,
            format!("no node {external} in this repo's Shard"),
        )),
        Ok(Err(e)) => Err(error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e:#}"),
        )),
        Err(e) => Err(error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("verb task panicked: {e}"),
        )),
    }
}

/// Resolve a repo qualifier to the local path of a Shard's verified
/// graph segment — the shared front half of every Verb. The pointer is
/// re-resolved on every call, so a swap is picked up by the next query
/// without a restart; `pinned` (from a pagination cursor) bypasses the
/// pointer and reads that exact immutable revision instead. Errors come
/// back as ready-to-return responses: unknown repos, never-indexed
/// repos, and expired cursors are the client's to hear about, not 500s.
async fn resolve_shard(
    state: &AppState,
    qualifier: &str,
    pinned: Option<String>,
) -> Result<(std::path::PathBuf, String), Response> {
    let target = match state.control.verb_target(qualifier).await {
        Ok(target) => target,
        Err(e) => {
            return Err(error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{e:#}"),
            ));
        }
    };
    let Some(target) = target else {
        return Err(error_json(
            StatusCode::NOT_FOUND,
            format!("no indexed repository matches {qualifier:?}"),
        ));
    };
    let from_cursor = pinned.is_some();
    let revision = match pinned.or(target.revision) {
        Some(revision) => revision,
        None => {
            return Err(error_json(
                StatusCode::NOT_FOUND,
                format!("{qualifier} is registered but not yet indexed; try again shortly"),
            ));
        }
    };
    match state.shards.graph_path(target.repo_id, &revision).await {
        Ok(path) => Ok((path, revision)),
        // A pinned revision that storage no longer holds is a cursor
        // outliving its Shard (GC, a forged or mistyped cursor): the
        // client must restart the traversal, the server is fine.
        Err(e) if from_cursor && e.downcast_ref::<yg_shard::RevisionMissing>().is_some() => {
            Err(error_json(
                StatusCode::GONE,
                "this cursor's Shard revision is no longer available; \
                 restart the traversal without a cursor",
            ))
        }
        // A revision published under an older index schema: a cursor
        // that outlived a deploy has simply expired; a current pointer
        // is already queued for re-indexing (worker boot requeues every
        // outdated Shard), so the client should retry, not despair.
        Err(e) if e.downcast_ref::<yg_shard::SchemaOutdated>().is_some() => {
            if from_cursor {
                Err(error_json(
                    StatusCode::GONE,
                    "this cursor's Shard revision predates the current index schema; \
                     restart the traversal without a cursor",
                ))
            } else {
                Err(error_json(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "this repo's Shard predates the current index schema and is being \
                     re-indexed; try again shortly",
                ))
            }
        }
        Err(e) => Err(error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e:#}"),
        )),
    }
}

#[derive(Deserialize)]
struct AddRepoRequest {
    url: String,
    /// Shallow-clone override; omitted = full history.
    depth: Option<i32>,
}

#[derive(Serialize)]
struct AddRepoResponse {
    slug: String,
    created: bool,
    /// False when a fetch was already pending — nothing new was queued.
    fetch_queued: bool,
}

async fn admin_repo_add(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddRepoRequest>,
) -> Response {
    if let Some(depth) = req.depth
        && depth < 1
    {
        return error_json(
            StatusCode::BAD_REQUEST,
            format!("depth must be a positive number of commits (got {depth})"),
        );
    }
    let locator = match RepoLocator::parse(&req.url) {
        Ok(locator) => locator,
        Err(reason) => return error_json(StatusCode::BAD_REQUEST, reason),
    };
    // Every node id this repo will ever mint embeds its qualifier
    // (RFC 0001 §5); a qualifier the id grammar can't address — an
    // IPv6 host, a path with a stray colon — would index a repo no
    // query could reach. Refused here, with the reason, instead.
    let qualifier = yg_control::repo_qualifier(&locator.base_url, &locator.slug);
    if !yg_verbs::addressable_qualifier(&qualifier) {
        return error_json(
            StatusCode::BAD_REQUEST,
            format!(
                "{} maps to repo qualifier {qualifier:?}, which node ids cannot address \
                 (it contains a colon outside a numeric port); \
                 use a hostname-based URL without colons in its path",
                req.url
            ),
        );
    }
    let outcome = state
        .control
        .add_repo(yg_control::AddRepo {
            forge_kind: locator.kind.as_str(),
            base_url: &locator.base_url,
            token_env: locator.kind.token_env(),
            slug: &locator.slug,
            fetch_depth: req.depth,
        })
        .await;
    match outcome {
        Ok(outcome) => (
            if outcome.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(AddRepoResponse {
                slug: locator.slug,
                created: outcome.created,
                fetch_queued: outcome.fetch_queued,
            }),
        )
            .into_response(),
        // The same host/slug registered through a different forge URL
        // (http vs https, say) is the caller's collision to resolve.
        Err(e) if e.downcast_ref::<yg_control::QualifierConflict>().is_some() => {
            error_json(StatusCode::CONFLICT, format!("{e}"))
        }
        Err(e) => error_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Serialize)]
struct AdminStatusResponse {
    repos: Vec<AdminRepoStatus>,
}

#[derive(Serialize)]
struct AdminRepoStatus {
    slug: String,
    forge: String,
    last_synced_commit: Option<String>,
    sync: JobStatus,
    index: JobStatus,
    /// The repo's current Shard; null until first indexed.
    shard: Option<ShardStatus>,
}

/// One pipeline stage's position, as admin status reports it for both
/// sync and index.
#[derive(Serialize)]
struct JobStatus {
    state: &'static str,
    attempts: i32,
    last_error: Option<String>,
}

#[derive(Serialize)]
struct ShardStatus {
    revision: String,
    nodes: i64,
    edges: i64,
}

async fn admin_status(State(state): State<Arc<AppState>>) -> Response {
    let repos = match state.control.admin_status().await {
        Ok(repos) => repos,
        Err(e) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let repos = repos
        .into_iter()
        .map(|r| AdminRepoStatus {
            sync: JobStatus {
                state: job_state(
                    r.job_state.as_deref(),
                    r.attempts,
                    r.last_synced_commit.is_some(),
                    StageWords {
                        active: "syncing",
                        done: "synced",
                        never_ran: "registered",
                    },
                ),
                attempts: r.attempts,
                last_error: r.last_error,
            },
            index: JobStatus {
                state: job_state(
                    r.index_job_state.as_deref(),
                    r.index_attempts,
                    r.shard_revision.is_some(),
                    StageWords {
                        active: "indexing",
                        done: "indexed",
                        never_ran: "pending",
                    },
                ),
                attempts: r.index_attempts,
                last_error: r.index_last_error,
            },
            shard: r.shard_revision.map(|revision| ShardStatus {
                revision,
                // Set together with the revision when a Shard is recorded.
                nodes: r.shard_node_count.unwrap_or(0),
                edges: r.shard_edge_count.unwrap_or(0),
            }),
            slug: r.slug,
            forge: r.forge,
            last_synced_commit: r.last_synced_commit,
        })
        .collect();
    Json(AdminStatusResponse { repos }).into_response()
}

/// The stage-specific words [`job_state`] fills in: what to call a
/// leased job, a stage that finished, and one that never ran.
struct StageWords {
    active: &'static str,
    done: &'static str,
    never_ran: &'static str,
}

/// Collapse a pipeline stage's queue position into the one word
/// `yg admin status` shows for it. `attempts` only ever rises above zero
/// through failures (`fail_*` re-queues with a backoff), so a queued job
/// with attempts is a retry, not a first run.
fn job_state(
    job_state: Option<&str>,
    attempts: i32,
    has_output: bool,
    words: StageWords,
) -> &'static str {
    match (job_state, attempts, has_output) {
        (Some("leased"), ..) => words.active,
        (Some("queued"), 0, _) => "queued",
        (Some("queued"), ..) => "retrying",
        (Some(_), ..) => "unknown",
        (None, _, true) => words.done,
        (None, _, false) => words.never_ran,
    }
}

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    uptime_seconds: u64,
    repos_indexed: i64,
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    match state.control.indexed_repo_count().await {
        Ok(repos_indexed) => Json(StatusResponse {
            version: env!("CARGO_PKG_VERSION"),
            uptime_seconds: state.started.elapsed().as_secs(),
            repos_indexed,
        })
        .into_response(),
        Err(e) => error_json(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    checks: HealthChecks,
}

#[derive(Serialize)]
struct HealthChecks {
    postgres: String,
    object_store: String,
}

async fn healthz(State(state): State<Arc<AppState>>) -> (StatusCode, Json<HealthResponse>) {
    let (postgres, object_store) = tokio::join!(
        state.control.ping(),
        probe_object_store(state.store.as_ref())
    );
    let all_ok = postgres.is_ok() && object_store.is_ok();

    let check = |r: &anyhow::Result<()>| match r {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("error: {e:#}"),
    };
    let body = HealthResponse {
        status: if all_ok { "ok" } else { "degraded" },
        version: env!("CARGO_PKG_VERSION"),
        checks: HealthChecks {
            postgres: check(&postgres),
            object_store: check(&object_store),
        },
    };
    let code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body))
}

#[cfg(test)]
mod tests {
    //! Cross-crate drift guards: the id grammar (yg-verbs) and the
    //! filter vocabulary (yg-verbs) each duplicate vocabulary that the
    //! Shard writer (yg-shard) owns, on purpose — the read path doesn't
    //! depend on the artifact writer. yg-api is the one crate that sees
    //! both, so these are the tests that catch a node or edge kind added
    //! to yg-shard but never taught to the read path.

    /// Every node kind the Shard writer mints must produce an external
    /// id the read path can parse and round-trip — otherwise a node is
    /// stored and counted but its `node`/`neighbors` ids 400 as
    /// malformed (this is exactly how `pkg:` would have regressed).
    #[test]
    fn every_node_kind_prefix_round_trips_through_the_id_grammar() {
        let repo = "github.com/acme/widgets";
        for kind in yg_shard::NodeKind::ALL {
            // A representative local part per prefix; the grammar cares
            // only about the prefix and a non-empty local part.
            let local = match kind.id_prefix() {
                "sym" => "cmd/main.go#Hello".to_string(),
                other => format!("{other}-local/part"),
            };
            let external = format!("{}:{repo}:{local}", kind.id_prefix());
            let parsed = yg_verbs::VerbId::parse(&external)
                .unwrap_or_else(|e| panic!("{kind:?} id {external:?} must parse: {e}"));
            assert_eq!(parsed.repo, repo, "{kind:?}");
            assert_eq!(parsed.external(), external, "{kind:?} must round-trip");
        }
    }

    use super::SearchHit;

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
        use super::merge_paginate;
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
        use super::merge_paginate;
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
        use super::{MAX_SEARCH_WINDOW, clamped_page_limit};
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
        use super::{SearchTarget, dedup_targets};
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
    /// statuses `resolve_shard` uses: a cursor outliving its Shard is the
    /// client's to restart (410), a re-indexing Shard is a retry (503).
    #[test]
    fn search_shard_errors_map_to_client_statuses() {
        use super::map_search_shard_error;
        use axum::http::StatusCode;
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
            map_search_shard_error(missing(), true).status(),
            StatusCode::GONE
        );
        assert_eq!(
            map_search_shard_error(outdated(), true).status(),
            StatusCode::GONE
        );
        assert_eq!(
            map_search_shard_error(outdated(), false).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // A fresh search resolves a current pointer, so a missing revision
        // there is an unexpected server fault, not a client-expired cursor.
        assert_eq!(
            map_search_shard_error(missing(), false).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// The `kinds` filter is validated against the node-kind vocabulary:
    /// an empty list is ambiguous, an unknown kind names the vocabulary.
    #[test]
    fn parse_search_kinds_validates_the_vocabulary() {
        use super::parse_search_kinds;
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

    /// The `edge_kinds` filter vocabulary must be exactly the set of
    /// edge kinds the Shard writer emits — no more (a filter for a kind
    /// no Shard holds), no fewer (a real kind a client can't filter to).
    #[test]
    fn known_edge_kinds_match_the_writer_exactly() {
        let mut written: Vec<&str> = yg_shard::EdgeKind::ALL.iter().map(|k| k.as_str()).collect();
        written.sort_unstable();
        let mut filterable: Vec<&str> = yg_verbs::KNOWN_EDGE_KINDS.to_vec();
        filterable.sort_unstable();
        assert_eq!(
            written, filterable,
            "yg_verbs::KNOWN_EDGE_KINDS must mirror yg_shard::EdgeKind"
        );
    }
}
