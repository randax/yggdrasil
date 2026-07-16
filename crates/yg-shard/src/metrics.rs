//! Prometheus instrumentation for the local Shard cache boundary.

use std::fmt::Write as _;

use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;

const CACHE_HITS: &str = "yggdrasil_shard_cache_hits";
const CACHE_MISSES: &str = "yggdrasil_shard_cache_misses";
const CACHE_EVICTIONS: &str = "yggdrasil_shard_cache_evictions";
const CACHE_CAPACITY_EVICTIONS: &str = "yggdrasil_shard_cache_capacity_evictions";

/// A typed cache artifact used for metric labels and diagnostics.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum Artifact {
    Manifest,
    Graph,
    Fts,
}

impl Artifact {
    const ALL: [Self; 3] = [Self::Manifest, Self::Graph, Self::Fts];

    const fn label(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Graph => "graph",
            Self::Fts => "fts",
        }
    }

    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Graph => "graph segment",
            Self::Fts => "fts segment",
        }
    }
}

impl EncodeLabelValue for Artifact {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> Result<(), std::fmt::Error> {
        encoder.write_str(self.label())
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ArtifactLabels {
    artifact: Artifact,
}

type ArtifactCounters = Family<ArtifactLabels, Counter>;

/// Cloneable collectors for local Shard-cache behavior.
#[derive(Clone)]
pub struct Metrics {
    hits: ArtifactCounters,
    misses: ArtifactCounters,
    evictions: ArtifactCounters,
    capacity_evictions: ArtifactCounters,
}

impl Metrics {
    /// Create and register the Shard-cache collectors in the server registry.
    pub fn registered(registry: &mut Registry) -> Self {
        let metrics = Self::unregistered();
        registry.register(CACHE_HITS, "Shard-cache hits.", metrics.hits.clone());
        registry.register(CACHE_MISSES, "Shard-cache misses.", metrics.misses.clone());
        registry.register(
            CACHE_EVICTIONS,
            "Corrupt Shard-cache artifacts rejected.",
            metrics.evictions.clone(),
        );
        registry.register(
            CACHE_CAPACITY_EVICTIONS,
            "Shard-cache artifacts removed to enforce the configured byte capacity.",
            metrics.capacity_evictions.clone(),
        );
        metrics
    }

    /// Create collectors without registering them for exposition.
    pub fn unregistered() -> Self {
        let metrics = Self {
            hits: Family::default(),
            misses: Family::default(),
            evictions: Family::default(),
            capacity_evictions: Family::default(),
        };
        for artifact in Artifact::ALL {
            let labels = ArtifactLabels { artifact };
            let _ = metrics.hits.get_or_create(&labels);
            let _ = metrics.misses.get_or_create(&labels);
            let _ = metrics.evictions.get_or_create(&labels);
            let _ = metrics.capacity_evictions.get_or_create(&labels);
        }
        metrics
    }

    pub(crate) fn hit(&self, artifact: Artifact) {
        self.hits.get_or_create(&ArtifactLabels { artifact }).inc();
    }

    pub(crate) fn miss(&self, artifact: Artifact) {
        self.misses
            .get_or_create(&ArtifactLabels { artifact })
            .inc();
    }

    pub(crate) fn eviction(&self, artifact: Artifact) {
        self.evictions
            .get_or_create(&ArtifactLabels { artifact })
            .inc();
    }

    pub(crate) fn capacity_eviction(&self, artifact: Artifact) {
        self.capacity_evictions
            .get_or_create(&ArtifactLabels { artifact })
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::encoding::text::encode;

    use super::*;

    #[test]
    fn counters_are_labeled_by_typed_artifact() {
        let mut registry = Registry::default();
        let metrics = Metrics::registered(&mut registry);
        metrics.hit(Artifact::Graph);
        metrics.miss(Artifact::Fts);
        metrics.eviction(Artifact::Manifest);
        metrics.capacity_eviction(Artifact::Graph);

        let mut text = String::new();
        encode(&mut text, &registry).unwrap();
        for series in [
            "yggdrasil_shard_cache_hits_total{artifact=\"graph\"} 1",
            "yggdrasil_shard_cache_misses_total{artifact=\"fts\"} 1",
            "yggdrasil_shard_cache_evictions_total{artifact=\"manifest\"} 1",
            "yggdrasil_shard_cache_capacity_evictions_total{artifact=\"graph\"} 1",
        ] {
            assert!(text.contains(series), "missing series {series:?}:\n{text}");
        }
    }
}
