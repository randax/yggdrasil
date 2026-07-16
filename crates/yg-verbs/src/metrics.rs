//! Prometheus instrumentation for the public Verb boundary.

use std::fmt::Write as _;
use std::time::Instant;

use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use crate::Verb;

const REQUEST_DURATION: &str = "yggdrasil_verb_request_duration_seconds";
const RESPONSE_SIZE: &str = "yggdrasil_verb_response_size_bytes";
// Sub-millisecond reads are common, while the upper buckets retain visibility
// into unusually slow requests without making the family excessively large.
const HISTOGRAM_BUCKETS: [f64; 14] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
const RESPONSE_SIZE_BUCKETS: [f64; 15] = [
    64.0,
    128.0,
    256.0,
    512.0,
    1_024.0,
    2_048.0,
    4_096.0,
    8_192.0,
    16_384.0,
    32_768.0,
    65_536.0,
    131_072.0,
    262_144.0,
    524_288.0,
    1_048_576.0,
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
type ResponseSize = Family<VerbLabels, Histogram, fn() -> Histogram>;

/// Cloneable collectors for Verb request latency and response size.
#[derive(Clone)]
pub struct Metrics {
    request_duration: RequestDuration,
    response_size: ResponseSize,
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
        registry.register(
            RESPONSE_SIZE,
            "Serialized successful Verb response payload size in bytes.",
            metrics.response_size.clone(),
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

    /// Record the compact JSON size of one successful typed Verb response.
    ///
    /// The engine calls this after its final response DTO is formed, which
    /// gives REST and MCP one shared payload measurement. Transport envelopes,
    /// headers, and error bodies are deliberately outside this observation.
    pub fn observe_response<T: serde::Serialize + ?Sized>(&self, verb: Verb, response: &T) {
        let mut counter = ByteCounter::default();
        match serde_json::to_writer(&mut counter, response) {
            Ok(()) => self
                .response_size
                .get_or_create(&VerbLabels { verb })
                .observe(counter.bytes as f64),
            Err(error) => {
                tracing::warn!(%error, verb = verb.label(), "measuring Verb response failed")
            }
        }
    }

    /// Create collectors without registering them for exposition.
    pub fn unregistered() -> Self {
        let metrics = Self {
            request_duration: Family::new_with_constructor(new_histogram as fn() -> Histogram),
            response_size: Family::new_with_constructor(
                new_response_size_histogram as fn() -> Histogram,
            ),
        };
        for verb in Verb::ALL {
            let _ = metrics.request_duration.get_or_create(&VerbLabels { verb });
            let _ = metrics.response_size.get_or_create(&VerbLabels { verb });
        }
        metrics
    }
}

fn new_histogram() -> Histogram {
    Histogram::new(HISTOGRAM_BUCKETS)
}

fn new_response_size_histogram() -> Histogram {
    Histogram::new(RESPONSE_SIZE_BUCKETS)
}

#[derive(Default)]
struct ByteCounter {
    bytes: usize,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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

    #[test]
    fn response_size_is_labeled_by_typed_verb_and_eagerly_initialized() {
        let mut registry = Registry::default();
        let metrics = Metrics::registered(&mut registry);
        metrics.observe_response(Verb::Search, &serde_json::json!({"hits": []}));

        let mut text = String::new();
        encode(&mut text, &registry).unwrap();
        assert!(text.contains("yggdrasil_verb_response_size_bytes_count{verb=\"search\"} 1"));
        assert!(text.contains("yggdrasil_verb_response_size_bytes_sum{verb=\"search\"} 11"));
        for verb in Verb::ALL {
            assert!(text.contains(&format!(
                "yggdrasil_verb_response_size_bytes_count{{verb=\"{}\"}}",
                verb.label()
            )));
        }
    }
}
