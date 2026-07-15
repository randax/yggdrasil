//! The read-only liveness surfaces: the authenticated `/v1/status`
//! summary and the unauthenticated `/healthz` readiness report.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use yg_shard::probe_object_store;
use yg_verbs::status::StatusResponse;

use crate::AppState;
use crate::error::ApiError;
use crate::wire::Wire;

/// Uptime changes every second, and response bodies must be
/// byte-identical for identical state (they become prompt-cache
/// history); volatile values ride in headers instead.
pub const UPTIME_HEADER: &str = "x-yggdrasil-uptime-seconds";

pub(crate) async fn status(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let repos_indexed = state.control.indexed_repo_count().await?;
    Ok((
        [(UPTIME_HEADER, state.started.elapsed().as_secs().to_string())],
        Wire(StatusResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            repos_indexed,
        }),
    )
        .into_response())
}

/// The unauthenticated readiness report: an overall verdict and a bare
/// ok/error per dependency. Anonymous callers never see failure detail —
/// connection errors carry hosts, ports, and bucket names — so the detail
/// goes to the server log instead.
#[derive(Serialize)]
pub(crate) struct HealthResponse {
    status: &'static str,
    checks: HealthChecks,
}

#[derive(Serialize)]
struct HealthChecks {
    postgres: &'static str,
    object_store: &'static str,
}

pub(crate) async fn healthz(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Wire<HealthResponse>) {
    let (postgres, object_store) = tokio::join!(
        state.control.ping(),
        probe_object_store(state.store.as_ref())
    );
    let all_ok = postgres.is_ok() && object_store.is_ok();

    let check = |name: &str, r: &anyhow::Result<()>| match r {
        Ok(()) => "ok",
        Err(e) => {
            tracing::warn!("health check {name} failed: {e:#}");
            "error"
        }
    };
    let body = HealthResponse {
        status: if all_ok { "ok" } else { "degraded" },
        checks: HealthChecks {
            postgres: check("postgres", &postgres),
            object_store: check("object_store", &object_store),
        },
    };
    let code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Wire(body))
}
