//! Prometheus instrumentation for the public Verb boundary.

use std::fmt::Write as _;
use std::time::Instant;

use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use crate::Verb;

const REQUEST_DURATION: &str = "yggdrasil_verb_request_duration_seconds";
const HISTOGRAM_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct VerbLabels {
    verb: Verb,
}

impl EncodeLabelValue for Verb {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> Result<(), std::fmt::Error> {
        encoder.write_str(self.label())
    }
}

type RequestDuration = Family<VerbLabels, Histogram, fn() -> Histogram>;

/// Cloneable collectors for Verb request latency.
#[derive(Clone)]
pub struct Metrics {
    request_duration: RequestDuration,
}

impl Metrics {
    /// Create and register the Verb collectors in the server registry.
    pub fn registered(registry: &mut Registry) -> Self {
        let metrics = Self::unregistered();
        registry.register(
            REQUEST_DURATION,
            "Verb request latency in seconds.",
            metrics.request_duration.clone(),
        );
        metrics
    }

    /// Start an observation for one typed Verb. Dropping the returned guard
    /// records elapsed time, including early error returns.
    pub fn timer(&self, verb: Verb) -> Timer {
        let histogram = self
            .request_duration
            .get_or_create(&VerbLabels { verb })
            .clone();
        Timer {
            histogram,
            started: Instant::now(),
        }
    }

    /// Create collectors without registering them for exposition.
    pub fn unregistered() -> Self {
        let metrics = Self {
            request_duration: Family::new_with_constructor(new_histogram as fn() -> Histogram),
        };
        for verb in Verb::ALL {
            let _ = metrics.request_duration.get_or_create(&VerbLabels { verb });
        }
        metrics
    }
}

fn new_histogram() -> Histogram {
    Histogram::new(HISTOGRAM_BUCKETS)
}

/// An in-flight Verb latency observation recorded when dropped.
#[must_use = "the timer must be retained for the duration of the Verb request"]
pub struct Timer {
    histogram: Histogram,
    started: Instant,
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.histogram.observe(self.started.elapsed().as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::encoding::text::encode;

    use super::*;

    #[test]
    fn latency_is_labeled_by_typed_verb() {
        let mut registry = Registry::default();
        let metrics = Metrics::registered(&mut registry);
        drop(metrics.timer(Verb::Search));

        let mut text = String::new();
        encode(&mut text, &registry).unwrap();
        assert!(text.contains("yggdrasil_verb_request_duration_seconds_count{verb=\"search\"} 1"));
    }
}
