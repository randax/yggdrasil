//! Prometheus collectors for the control-plane job queue.

use std::time::Instant;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

use crate::JobKind;

pub const JOB_QUEUE_DEPTH: &str = "yggdrasil_job_queue_depth";
pub const JOB_CLAIM_LATENCY_SECONDS: &str = "yggdrasil_job_claim_latency_seconds";
pub const JOB_OUTCOMES_TOTAL: &str = "yggdrasil_job_outcomes_total";
pub const JOB_DURATION_SECONDS: &str = "yggdrasil_job_duration_seconds";

const JOB_OUTCOMES_BASE: &str = "yggdrasil_job_outcomes";

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct KindLabels {
    kind: &'static str,
}

impl From<JobKind> for KindLabels {
    fn from(kind: JobKind) -> Self {
        Self {
            kind: kind.as_str(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabels {
    kind: &'static str,
    outcome: &'static str,
}

/// The terminal result of one claimed job execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    Success,
    Failure,
    Discarded,
}

impl JobOutcome {
    const ALL: [Self; 3] = [Self::Success, Self::Failure, Self::Discarded];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Discarded => "discarded",
        }
    }
}

/// Cloneable job-queue collectors, either registered for exposition or
/// deliberately unregistered for call sites that do not expose metrics.
#[derive(Clone)]
pub struct Metrics {
    queue_depth: Family<KindLabels, Gauge>,
    claim_latency: Family<KindLabels, Histogram>,
    outcomes: Family<OutcomeLabels, Counter>,
    durations: Family<KindLabels, Histogram>,
}

impl Metrics {
    /// Construct and register all job collectors in `registry`.
    pub fn registered(registry: &mut Registry) -> Self {
        let metrics = Self::unregistered();
        registry.register(
            JOB_QUEUE_DEPTH,
            "Current non-terminal job queue depth.",
            metrics.queue_depth.clone(),
        );
        registry.register(
            JOB_CLAIM_LATENCY_SECONDS,
            "Seconds from a job becoming eligible until it is claimed.",
            metrics.claim_latency.clone(),
        );
        registry.register(
            JOB_OUTCOMES_BASE,
            "Terminal outcomes of claimed jobs.",
            metrics.outcomes.clone(),
        );
        registry.register(
            JOB_DURATION_SECONDS,
            "Seconds spent executing and settling claimed jobs.",
            metrics.durations.clone(),
        );
        metrics
    }

    /// Construct collectors without registering them for exposition.
    pub fn unregistered() -> Self {
        let metrics = Self {
            queue_depth: Family::default(),
            claim_latency: Family::new_with_constructor(new_histogram),
            outcomes: Family::default(),
            durations: Family::new_with_constructor(new_histogram),
        };
        for kind in JobKind::ALL {
            let kind_labels = KindLabels::from(kind);
            let _ = metrics.queue_depth.get_or_create(&kind_labels);
            let _ = metrics.claim_latency.get_or_create(&kind_labels);
            let _ = metrics.durations.get_or_create(&kind_labels);
            for outcome in JobOutcome::ALL {
                let _ = metrics.outcomes.get_or_create(&OutcomeLabels {
                    kind: kind.as_str(),
                    outcome: outcome.as_str(),
                });
            }
        }
        metrics
    }

    pub(crate) fn set_queue_depth(&self, kind: JobKind, depth: u64) {
        let depth = i64::try_from(depth).unwrap_or(i64::MAX);
        self.queue_depth.get_or_create(&kind.into()).set(depth);
    }

    pub(crate) fn observe_claim_latency(&self, kind: JobKind, seconds: f64) {
        self.claim_latency
            .get_or_create(&kind.into())
            .observe(seconds);
    }

    /// Start timing a claimed job. Dropping the timer does not emit a
    /// partial observation; callers finish it with a typed outcome.
    pub fn start_job(&self, kind: JobKind) -> JobTimer {
        JobTimer {
            metrics: self.clone(),
            kind,
            started: Instant::now(),
        }
    }

    fn observe_job(&self, kind: JobKind, outcome: JobOutcome, seconds: f64) {
        self.outcomes
            .get_or_create(&OutcomeLabels {
                kind: kind.as_str(),
                outcome: outcome.as_str(),
            })
            .inc();
        self.durations.get_or_create(&kind.into()).observe(seconds);
    }
}

fn new_histogram() -> Histogram {
    Histogram::new(exponential_buckets(0.005, 2.0, 24))
}

/// An in-flight job duration observation.
pub struct JobTimer {
    metrics: Metrics,
    kind: JobKind,
    started: Instant,
}

impl JobTimer {
    /// Record the duration and terminal outcome exactly once.
    pub fn finish(self, outcome: JobOutcome) {
        self.metrics
            .observe_job(self.kind, outcome, self.started.elapsed().as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::encoding::text::encode;

    use super::*;

    #[test]
    fn registered_collectors_use_typed_job_labels() {
        let mut registry = Registry::default();
        let metrics = Metrics::registered(&mut registry);
        metrics.set_queue_depth(JobKind::Fetch, 3);
        metrics.observe_claim_latency(JobKind::Fetch, 0.25);
        metrics.observe_job(JobKind::Index, JobOutcome::Success, 0.5);

        let mut body = String::new();
        encode(&mut body, &registry).unwrap();
        assert!(body.contains("yggdrasil_job_queue_depth{kind=\"fetch\"} 3"));
        assert!(
            body.contains("yggdrasil_job_outcomes_total{kind=\"index\",outcome=\"success\"} 1")
        );
        assert!(body.contains("yggdrasil_job_claim_latency_seconds_count{kind=\"fetch\"} 1"));
    }

    #[test]
    fn unregistered_collectors_do_not_leak_into_a_registry() {
        let registry = Registry::default();
        let metrics = Metrics::unregistered();
        metrics.set_queue_depth(JobKind::Fetch, 1);
        let mut body = String::new();
        encode(&mut body, &registry).unwrap();
        assert!(!body.contains(JOB_QUEUE_DEPTH));
    }
}
