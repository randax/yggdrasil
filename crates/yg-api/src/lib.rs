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
