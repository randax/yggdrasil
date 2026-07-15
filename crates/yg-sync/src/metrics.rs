//! Prometheus collectors for Forge polling freshness.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

pub const FORGE_POLL_LAG_SECONDS: &str = "yggdrasil_forge_poll_lag_seconds";
pub const FORGE_POLL_LAG_OBSERVATIONS_SECONDS: &str =
    "yggdrasil_forge_poll_lag_observations_seconds";

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ForgeLabels {
    forge: String,
}

/// Cloneable Forge poll-lag collectors.
#[derive(Clone)]
pub struct Metrics {
    poll_lag: Family<ForgeLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,
    poll_lag_observations: Family<ForgeLabels, Histogram>,
}

impl Metrics {
    /// Construct and register all sync collectors in `registry`.
    pub fn registered(registry: &mut Registry) -> Self {
        let metrics = Self::unregistered();
        registry.register(
            FORGE_POLL_LAG_SECONDS,
            "Most recently observed poll lag in seconds for each Forge.",
            metrics.poll_lag.clone(),
        );
        registry.register(
            FORGE_POLL_LAG_OBSERVATIONS_SECONDS,
            "Distribution of repo poll lag in seconds for each Forge.",
            metrics.poll_lag_observations.clone(),
        );
        metrics
    }

    /// Construct collectors without registering them for exposition.
    pub fn unregistered() -> Self {
        Self {
            poll_lag: Family::default(),
            poll_lag_observations: Family::new_with_constructor(|| {
                Histogram::new(exponential_buckets(0.005, 2.0, 24))
            }),
        }
    }

    pub(crate) fn observe_poll_lag(&self, forge: &str, seconds: f64) {
        let labels = ForgeLabels {
            forge: forge.to_owned(),
        };
        self.poll_lag.get_or_create(&labels).set(seconds);
        self.poll_lag_observations
            .get_or_create(&labels)
            .observe(seconds);
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::encoding::text::encode;

    use super::*;

    #[test]
    fn poll_lag_is_exposed_under_the_unique_forge_label() {
        let mut registry = Registry::default();
        let metrics = Metrics::registered(&mut registry);
        metrics.observe_poll_lag("https://github.com", 1.5);

        let mut body = String::new();
        encode(&mut body, &registry).unwrap();
        assert!(
            body.contains("yggdrasil_forge_poll_lag_seconds{forge=\"https://github.com\"} 1.5")
        );
        assert!(body.contains(
            "yggdrasil_forge_poll_lag_observations_seconds_count{forge=\"https://github.com\"} 1"
        ));
    }

    #[test]
    fn unregistered_collectors_are_not_gathered() {
        let registry = Registry::default();
        Metrics::unregistered().observe_poll_lag("https://example.com", 0.0);
        let mut body = String::new();
        encode(&mut body, &registry).unwrap();
        assert!(!body.contains(FORGE_POLL_LAG_SECONDS));
    }
}
