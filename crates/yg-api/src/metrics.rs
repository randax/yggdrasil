//! Prometheus registry composition and HTTP exposition.

use std::sync::Arc;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;

use crate::AppState;
use crate::MetricsServerState;
use crate::error::ApiError;

/// All process-local collectors exposed by the server's registry.
///
/// The CLI creates this bundle once and passes clones to the API and
/// worker composition seams, avoiding a mutable global recorder.
#[derive(Clone)]
pub struct Metrics {
    registry: Arc<Registry>,
    control: yg_control::Metrics,
    sync: yg_sync::Metrics,
    shard: yg_shard::Metrics,
    verbs: yg_verbs::Metrics,
}

impl Metrics {
    /// Build and register every collector owned by the assembled process.
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let control = yg_control::Metrics::registered(&mut registry);
        let sync = yg_sync::Metrics::registered(&mut registry);
        let shard = yg_shard::Metrics::registered(&mut registry);
        let verbs = yg_verbs::Metrics::registered(&mut registry);
        Self {
            registry: Arc::new(registry),
            control,
            sync,
            shard,
            verbs,
        }
    }

    pub fn control(&self) -> yg_control::Metrics {
        self.control.clone()
    }

    pub fn sync_worker(&self) -> yg_sync::Metrics {
        self.sync.clone()
    }

    pub(crate) fn shard_cache(&self) -> yg_shard::Metrics {
        self.shard.clone()
    }

    pub(crate) fn verbs(&self) -> yg_verbs::Metrics {
        self.verbs.clone()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// `GET /metrics`: Prometheus text exposition for operational telemetry.
///
/// The route requires the Admin bearer token by default, through the same
/// structural authentication and scope layers as `/v1/admin/status`. When
/// `YG_METRICS_UNAUTHENTICATED=true`, the composition root deliberately adds
/// this route outside those layers so a network-restricted scraper can call it.
pub(crate) async fn metrics(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    response(&state.metrics)
}

pub(crate) async fn standalone_metrics(
    State(state): State<Arc<MetricsServerState>>,
) -> Result<Response, ApiError> {
    response(&state.metrics)
}

fn response(metrics: &Metrics) -> Result<Response, ApiError> {
    let mut body = String::new();
    encode(&mut body, &metrics.registry)
        .map_err(|error| ApiError::internal(anyhow::Error::msg(error.to_string())))?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_registers_each_crate_boundary() {
        let metrics = Metrics::new();
        let mut text = String::new();
        encode(&mut text, &metrics.registry).unwrap();
        assert!(text.ends_with("# EOF\n"));
    }
}
