//! Prometheus collectors for Forge polling freshness.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

pub const FORGE_POLL_LAG_OBSERVATIONS_SECONDS: &str =
    "yggdrasil_forge_poll_lag_observations_seconds";

// Poll lag is operationally useful from sub-second jitter through sustained
// delays measured in minutes. Larger delays remain visible in the +Inf bucket.
const POLL_LAG_BUCKETS: [f64; 11] = [
    0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0,
];

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ForgeLabels {
    forge: String,
}

/// Cloneable Forge poll-lag collectors.
#[derive(Clone)]
pub struct Metrics {
    poll_lag_observations: Family<ForgeLabels, Histogram>,
}

impl Metrics {
    /// Construct and register all sync collectors in `registry`.
    pub fn registered(registry: &mut Registry) -> Self {
        let metrics = Self::unregistered();
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
            poll_lag_observations: Family::new_with_constructor(|| {
                Histogram::new(POLL_LAG_BUCKETS)
            }),
        }
    }

    pub(crate) fn observe_poll_lag(&self, forge: &str, seconds: f64) {
        let labels = ForgeLabels {
            forge: forge.to_owned(),
        };
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
    fn poll_lag_distribution_is_exposed_under_the_unique_forge_label() {
        let mut registry = Registry::default();
        let metrics = Metrics::registered(&mut registry);
        metrics.observe_poll_lag("https://github.com", 1.5);

        let mut body = String::new();
        encode(&mut body, &registry).unwrap();
        assert!(body.contains(
            "yggdrasil_forge_poll_lag_observations_seconds_count{forge=\"https://github.com\"} 1"
        ));
        assert!(
            body.contains(
                "yggdrasil_forge_poll_lag_observations_seconds_bucket{le=\"900.0\",forge=\"https://github.com\"} 1"
            ),
            "{body}"
        );
        assert!(!body.contains("# HELP yggdrasil_forge_poll_lag_seconds "));
    }

    #[test]
    fn unregistered_collectors_are_not_gathered() {
        let registry = Registry::default();
        Metrics::unregistered().observe_poll_lag("https://example.com", 0.0);
        let mut body = String::new();
        encode(&mut body, &registry).unwrap();
        assert!(!body.contains(FORGE_POLL_LAG_OBSERVATIONS_SECONDS));
    }
}
