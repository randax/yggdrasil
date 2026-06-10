//! REST + MCP server.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::State;
use axum::http::StatusCode;
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

/// A booted Index Server, listening until dropped or the process exits.
pub struct RunningServer {
    local_addr: SocketAddr,
    _handle: JoinHandle<()>,
}

impl RunningServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

struct AppState {
    control: ControlPlane,
    store: Box<dyn ObjectStore>,
}

/// Boot the Index Server: connect to the control plane, verify object
/// storage, and start serving.
pub async fn serve(config: ServerConfig) -> anyhow::Result<RunningServer> {
    let control = ControlPlane::connect(&config.database_url).await?;

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

    let state = Arc::new(AppState { control, store });
    let app = Router::new()
        .route("/healthz", get(healthz))
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

    Ok(RunningServer {
        local_addr,
        _handle: handle,
    })
}

/// Cheap reachability check that distinguishes "bucket missing/unreachable"
/// from "bucket empty": a delimited list succeeds on an empty bucket but
/// errors when the bucket doesn't exist.
async fn probe_object_store(store: &dyn ObjectStore) -> anyhow::Result<()> {
    store.list_with_delimiter(None).await?;
    Ok(())
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
