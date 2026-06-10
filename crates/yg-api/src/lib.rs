//! REST + MCP server.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use yg_control::ControlPlane;

pub struct ServerConfig {
    pub listen: SocketAddr,
    pub database_url: String,
    pub object_store: ObjectStoreConfig,
    pub bootstrap_token: String,
}

pub struct ObjectStoreConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
}

impl ServerConfig {
    /// Build from `YG_*` environment variables. Everything defaults to the
    /// in-repo dev compose stack except the bootstrap Admin token, which
    /// has no safe default.
    pub fn from_env() -> anyhow::Result<Self> {
        fn var_or(key: &str, default: &str) -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_string())
        }
        let bootstrap_token = std::env::var("YG_BOOTSTRAP_TOKEN")
            .ok()
            .filter(|token| !token.trim().is_empty())
            .context(
                "YG_BOOTSTRAP_TOKEN must be set to a non-empty token; \
                 the server refuses to boot without an Admin token",
            )?;
        Ok(Self {
            listen: var_or("YG_LISTEN", "127.0.0.1:7311")
                .parse()
                .context("parsing YG_LISTEN as host:port")?,
            database_url: var_or(
                "YG_DATABASE_URL",
                "postgres://yggdrasil:yggdrasil@localhost:5432/yggdrasil",
            ),
            object_store: ObjectStoreConfig {
                endpoint: var_or("YG_S3_ENDPOINT", "http://localhost:9000"),
                bucket: var_or("YG_S3_BUCKET", "yggdrasil"),
                access_key: var_or("YG_S3_ACCESS_KEY", "yggdrasil"),
                secret_key: var_or("YG_S3_SECRET_KEY", "yggdrasil"),
                region: var_or("YG_S3_REGION", "us-east-1"),
            },
            bootstrap_token,
        })
    }
}

/// A booted Index Server, listening until dropped or the process exits.
pub struct RunningServer {
    local_addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl RunningServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run until the server task ends (it normally never does).
    pub async fn wait(self) -> anyhow::Result<()> {
        self.handle.await.context("server task panicked")
    }
}

struct AppState {
    control: ControlPlane,
    store: Box<dyn ObjectStore>,
    bootstrap_token: String,
    started: std::time::Instant,
}

/// Boot the Index Server: connect to the control plane, verify object
/// storage, and start serving.
pub async fn serve(config: ServerConfig) -> anyhow::Result<RunningServer> {
    let control = ControlPlane::connect_and_migrate(&config.database_url).await?;

    let store: Box<dyn ObjectStore> = Box::new(
        AmazonS3Builder::new()
            .with_endpoint(&config.object_store.endpoint)
            .with_bucket_name(&config.object_store.bucket)
            .with_access_key_id(&config.object_store.access_key)
            .with_secret_access_key(&config.object_store.secret_key)
            .with_region(&config.object_store.region)
            .with_allow_http(true)
            .build()
            .context("configuring object store client")?,
    );
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
        .nest("/v1", Router::new().route("/status", get(status)))
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
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("server exited: {e}");
        }
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
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| {
            presented
                .as_bytes()
                .ct_eq(state.bootstrap_token.as_bytes())
                .into()
        });
    if authorized {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "missing or invalid bearer token"})),
        )
            .into_response()
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e:#}")})),
        )
            .into_response(),
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
    let postgres = state.control.ping().await;
    let object_store = probe_object_store(state.store.as_ref()).await;
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
