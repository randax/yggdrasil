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
pub use yg_shard::ObjectStoreConfig;
use yg_sync::RepoLocator;

pub struct ServerConfig {
    pub listen: SocketAddr,
    pub database_url: String,
    pub object_store: ObjectStoreConfig,
    pub bootstrap_token: String,
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

/// Cheap reachability check that distinguishes "bucket missing/unreachable"
/// from "bucket empty": a delimited list succeeds on an empty bucket but
/// errors when the bucket doesn't exist.
async fn probe_object_store(store: &dyn ObjectStore) -> anyhow::Result<()> {
    store.list_with_delimiter(None).await?;
    Ok(())
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
