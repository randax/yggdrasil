//! Running one leased job from claim to settlement.

use std::future::Future;
use std::marker::PhantomData;
use std::time::Duration;

use yg_control::{ControlPlane, JobKind, JobOutcome, JobTimer, LeasedJob};

use crate::lease::{LeaseShutdown, with_lease_heartbeat, with_lease_heartbeat_until_shutdown};
use crate::shutdown::Shutdown;

/// Observes one job attempt's outcome.
pub trait OutcomeTimer {
    fn finish(self, outcome: JobOutcome);

    /// The claim was released untouched, so there is no attempt to report.
    fn disarm(self);
}

impl OutcomeTimer for JobTimer {
    fn finish(self, outcome: JobOutcome) {
        JobTimer::finish(self, outcome);
    }

    fn disarm(self) {
        JobTimer::disarm(self);
    }
}

/// Where leased jobs of one kind are claimed and their leases kept.
pub trait JobQueue: Sync {
    type Job: Send + Sync;
    type Timer: OutcomeTimer + Send;
    const KIND: JobKind;

    fn lease(&self) -> Duration;
    fn slug(job: &Self::Job) -> &str;
    fn attempts(job: &Self::Job) -> i32;
    fn claim(&self) -> impl Future<Output = anyhow::Result<Option<Self::Job>>> + Send;
    fn start_timer(&self) -> Self::Timer;
    /// `false` means the job was reclaimed by another worker.
    fn renew(&self, job: &Self::Job) -> impl Future<Output = anyhow::Result<bool>> + Send;
    fn release(&self, job: &Self::Job) -> impl Future<Output = anyhow::Result<bool>> + Send;
    /// `false` means the lease lapsed, so the failure was not recorded.
    fn fail(
        &self,
        job: &Self::Job,
        error: &str,
    ) -> impl Future<Output = anyhow::Result<bool>> + Send;
}

/// The control plane's queue for job kind `J`.
pub struct ControlQueue<'a, J> {
    control: &'a ControlPlane,
    lease: Duration,
    kind: PhantomData<fn() -> J>,
}

impl<'a, J> ControlQueue<'a, J> {
    pub fn new(control: &'a ControlPlane, lease: Duration) -> Self {
        Self {
            control,
            lease,
            kind: PhantomData,
        }
    }
}

impl<J: LeasedJob> JobQueue for ControlQueue<'_, J> {
    type Job = J;
    type Timer = JobTimer;
    const KIND: JobKind = J::KIND;

    fn lease(&self) -> Duration {
        self.lease
    }

    fn slug(job: &J) -> &str {
        job.slug()
    }

    fn attempts(job: &J) -> i32 {
        job.attempts()
    }

    async fn claim(&self) -> anyhow::Result<Option<J>> {
        J::claim(self.control, self.lease).await
    }

    fn start_timer(&self) -> JobTimer {
        self.control.start_job(J::KIND)
    }

    async fn renew(&self, job: &J) -> anyhow::Result<bool> {
        self.control.renew_job(job, self.lease).await
    }

    async fn release(&self, job: &J) -> anyhow::Result<bool> {
        self.control.release_job(job).await
    }

    async fn fail(&self, job: &J, error: &str) -> anyhow::Result<bool> {
        self.control.fail_job(job, error).await
    }
}

/// What a worker does with a claimed job of kind `J`.
pub trait LeasedWork<J>: Sync {
    type Output: Send;

    /// Do the job. An `Err` is recorded against the job with backoff.
    fn run(&self, job: &J) -> impl Future<Output = anyhow::Result<Self::Output>> + Send;

    /// Apply finished work while the lease may still be held, and report
    /// the attempt's outcome. An `Err` means the control plane is unreachable.
    fn settle(
        &self,
        job: &J,
        output: Self::Output,
    ) -> impl Future<Output = anyhow::Result<JobOutcome>> + Send;
}

/// Claim and run one due job, returning whether there was a claim. A failed
/// run is recorded rather than returned: `Err` means the control plane is
/// unreachable. With a `shutdown`, a claim taken after shutdown began is
/// released untouched, and work past the cutoff has its lease returned fresh
/// to the queue.
pub async fn run_leased_job<Q, W>(
    queue: &Q,
    shutdown: Option<Shutdown>,
    work: &W,
) -> anyhow::Result<bool>
where
    Q: JobQueue,
    W: LeasedWork<Q::Job>,
{
    let Some(job) = queue.claim().await? else {
        return Ok(false);
    };
    let timer = queue.start_timer();
    let kind = Q::KIND.as_str();
    if shutdown.as_ref().is_some_and(|s| s.request().is_some()) {
        let released = queue.release(&job).await?;
        tracing::info!(kind, released, "released fresh claim for shutdown");
        timer.disarm();
        return Ok(true);
    }
    // Work such as a cold clone outlives the base lease; the heartbeat keeps
    // the job ours for as long as the work is alive.
    let renew = async || queue.renew(&job).await;
    let ran = if let Some(shutdown) = shutdown {
        let release = async || queue.release(&job).await;
        let run = work.run(&job);
        match with_lease_heartbeat_until_shutdown(queue.lease(), renew, release, shutdown, run)
            .await?
        {
            LeaseShutdown::Finished(ran) => ran,
            LeaseShutdown::Released => {
                timer.finish(JobOutcome::Discarded);
                return Ok(true);
            }
        }
    } else {
        with_lease_heartbeat(queue.lease(), renew, work.run(&job)).await
    };
    let outcome = match ran {
        Ok(output) => work.settle(&job, output).await?,
        Err(error) => record_failure(queue, &job, &error).await?,
    };
    timer.finish(outcome);
    Ok(true)
}

async fn record_failure<Q: JobQueue>(
    queue: &Q,
    job: &Q::Job,
    error: &anyhow::Error,
) -> anyhow::Result<JobOutcome> {
    let error = format!("{error:#}");
    let kind = Q::KIND.as_str();
    let slug = Q::slug(job);
    if queue.fail(job, &error).await? {
        let attempt = Q::attempts(job) + 1;
        tracing::warn!(kind, slug, attempt, error, "job failed");
        Ok(JobOutcome::Failure)
    } else {
        tracing::warn!(kind, slug, "lease lapsed during the job; failure discarded");
        Ok(JobOutcome::Discarded)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{ShutdownCause, shutdown_channel};

    const LEASE: Duration = Duration::from_secs(60);

    #[derive(Debug, Default, PartialEq)]
    struct Ledger {
        started_timers: usize,
        renewals: usize,
        releases: usize,
        failures: Vec<String>,
        outcome: Option<JobOutcome>,
        disarmed: bool,
    }

    /// Like the real timer, one dropped unsettled reports a failed attempt.
    struct FakeTimer {
        ledger: Arc<Mutex<Ledger>>,
        recorded: bool,
    }

    impl OutcomeTimer for FakeTimer {
        fn finish(mut self, outcome: JobOutcome) {
            self.recorded = true;
            self.ledger.lock().unwrap().outcome = Some(outcome);
        }

        fn disarm(mut self) {
            self.recorded = true;
            self.ledger.lock().unwrap().disarmed = true;
        }
    }

    impl Drop for FakeTimer {
        fn drop(&mut self) {
            if !self.recorded {
                self.ledger.lock().unwrap().outcome = Some(JobOutcome::Failure);
            }
        }
    }

    struct FakeQueue {
        job: Mutex<Option<u32>>,
        fail_applies: bool,
        ledger: Arc<Mutex<Ledger>>,
    }

    impl FakeQueue {
        fn with_job() -> Self {
            Self {
                job: Mutex::new(Some(7)),
                fail_applies: true,
                ledger: Arc::default(),
            }
        }

        fn empty() -> Self {
            Self {
                job: Mutex::new(None),
                ..Self::with_job()
            }
        }

        fn fenced() -> Self {
            Self {
                fail_applies: false,
                ..Self::with_job()
            }
        }

        fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
            self.ledger.lock().unwrap()
        }
    }

    impl JobQueue for FakeQueue {
        type Job = u32;
        type Timer = FakeTimer;
        const KIND: JobKind = JobKind::Fetch;

        fn lease(&self) -> Duration {
            LEASE
        }

        fn slug(_: &u32) -> &str {
            "acme/widgets"
        }

        fn attempts(_: &u32) -> i32 {
            0
        }

        async fn claim(&self) -> anyhow::Result<Option<u32>> {
            Ok(self.job.lock().unwrap().take())
        }

        fn start_timer(&self) -> FakeTimer {
            self.ledger().started_timers += 1;
            FakeTimer {
                ledger: self.ledger.clone(),
                recorded: false,
            }
        }

        async fn renew(&self, _: &u32) -> anyhow::Result<bool> {
            self.ledger().renewals += 1;
            Ok(true)
        }

        async fn release(&self, _: &u32) -> anyhow::Result<bool> {
            self.ledger().releases += 1;
            Ok(true)
        }

        async fn fail(&self, _: &u32, error: &str) -> anyhow::Result<bool> {
            self.ledger().failures.push(error.to_string());
            Ok(self.fail_applies)
        }
    }

    /// Work that sleeps, then yields `result`, and settles as `settles_as`.
    struct FakeWork {
        takes: Duration,
        result: Result<&'static str, &'static str>,
        settles_as: anyhow::Result<JobOutcome>,
        ran: Mutex<bool>,
        settled_with: Mutex<Option<String>>,
    }

    impl FakeWork {
        fn succeeding() -> Self {
            Self {
                takes: Duration::ZERO,
                result: Ok("output"),
                settles_as: Ok(JobOutcome::Success),
                ran: Mutex::new(false),
                settled_with: Mutex::new(None),
            }
        }

        fn failing() -> Self {
            Self {
                result: Err("clone exploded"),
                ..Self::succeeding()
            }
        }

        fn taking(takes: Duration) -> Self {
            Self {
                takes,
                ..Self::succeeding()
            }
        }

        fn ran(&self) -> bool {
            *self.ran.lock().unwrap()
        }

        fn settled_with(&self) -> Option<String> {
            self.settled_with.lock().unwrap().clone()
        }
    }

    impl LeasedWork<u32> for FakeWork {
        type Output = &'static str;

        async fn run(&self, _: &u32) -> anyhow::Result<&'static str> {
            *self.ran.lock().unwrap() = true;
            tokio::time::sleep(self.takes).await;
            self.result.map_err(anyhow::Error::msg)
        }

        async fn settle(&self, _: &u32, output: &'static str) -> anyhow::Result<JobOutcome> {
            *self.settled_with.lock().unwrap() = Some(output.to_string());
            match &self.settles_as {
                Ok(outcome) => Ok(*outcome),
                Err(error) => Err(anyhow::anyhow!("{error}")),
            }
        }
    }

    #[tokio::test]
    async fn an_empty_queue_reports_no_work_and_records_nothing() {
        let queue = FakeQueue::empty();
        let work = FakeWork::succeeding();

        let had_work = run_leased_job(&queue, None, &work).await.unwrap();

        assert!(!had_work);
        assert!(!work.ran());
        assert_eq!(*queue.ledger(), Ledger::default());
    }

    #[tokio::test]
    async fn finished_work_settles_and_records_the_settled_outcome() {
        let queue = FakeQueue::with_job();
        let work = FakeWork {
            settles_as: Ok(JobOutcome::Discarded),
            ..FakeWork::succeeding()
        };

        let had_work = run_leased_job(&queue, None, &work).await.unwrap();

        assert!(had_work);
        assert_eq!(work.settled_with().as_deref(), Some("output"));
        assert_eq!(queue.ledger().outcome, Some(JobOutcome::Discarded));
        assert!(queue.ledger().failures.is_empty());
    }

    #[tokio::test]
    async fn failed_work_is_recorded_without_settling() {
        let queue = FakeQueue::with_job();
        let work = FakeWork::failing();

        let had_work = run_leased_job(&queue, None, &work).await.unwrap();

        assert!(had_work, "a failed run is recorded, not returned");
        assert_eq!(queue.ledger().failures, ["clone exploded"]);
        assert_eq!(queue.ledger().outcome, Some(JobOutcome::Failure));
        assert_eq!(work.settled_with(), None);
    }

    #[tokio::test]
    async fn a_failure_after_the_lease_lapsed_is_discarded() {
        let queue = FakeQueue::fenced();
        let work = FakeWork::failing();

        run_leased_job(&queue, None, &work).await.unwrap();

        assert_eq!(queue.ledger().outcome, Some(JobOutcome::Discarded));
    }

    #[tokio::test]
    async fn a_settlement_error_propagates_and_reports_a_failed_attempt() {
        let queue = FakeQueue::with_job();
        let work = FakeWork {
            settles_as: Err(anyhow::anyhow!("control plane unreachable")),
            ..FakeWork::succeeding()
        };

        let error = run_leased_job(&queue, None, &work)
            .await
            .expect_err("Err means the control plane is unreachable");

        assert_eq!(error.to_string(), "control plane unreachable");
        assert_eq!(queue.ledger().outcome, Some(JobOutcome::Failure));
    }

    #[tokio::test]
    async fn a_claim_taken_during_shutdown_is_released_untouched() {
        let queue = FakeQueue::with_job();
        let work = FakeWork::succeeding();
        let (trigger, shutdown) = shutdown_channel();
        trigger.request(tokio::time::Instant::now(), ShutdownCause::Signal);

        let had_work = run_leased_job(&queue, Some(shutdown), &work).await.unwrap();

        assert!(had_work);
        assert!(!work.ran());
        assert_eq!(queue.ledger().releases, 1);
        assert!(queue.ledger().disarmed, "no attempt happened to report");
        assert_eq!(queue.ledger().outcome, None);
    }

    #[tokio::test(start_paused = true)]
    async fn work_past_the_shutdown_cutoff_is_released_and_discarded() {
        let queue = FakeQueue::with_job();
        let work = FakeWork::taking(Duration::from_secs(20));
        let (trigger, shutdown) = shutdown_channel();

        let run = run_leased_job(&queue, Some(shutdown), &work);
        let request = async {
            tokio::task::yield_now().await;
            trigger.request(
                tokio::time::Instant::now() + Duration::from_secs(5),
                ShutdownCause::Signal,
            );
        };
        let (had_work, ()) = tokio::join!(run, request);

        assert!(had_work.unwrap());
        assert_eq!(queue.ledger().releases, 1);
        assert_eq!(queue.ledger().outcome, Some(JobOutcome::Discarded));
        assert_eq!(work.settled_with(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn work_finished_before_the_shutdown_cutoff_still_settles() {
        let queue = FakeQueue::with_job();
        let work = FakeWork::taking(Duration::from_secs(2));
        let (trigger, shutdown) = shutdown_channel();

        let run = run_leased_job(&queue, Some(shutdown), &work);
        let request = async {
            tokio::task::yield_now().await;
            trigger.request(
                tokio::time::Instant::now() + Duration::from_secs(30),
                ShutdownCause::Signal,
            );
        };
        let (had_work, ()) = tokio::join!(run, request);

        assert!(had_work.unwrap());
        assert_eq!(queue.ledger().releases, 0);
        assert_eq!(queue.ledger().outcome, Some(JobOutcome::Success));
    }

    #[tokio::test(start_paused = true)]
    async fn long_work_renews_its_lease_every_third_of_the_lease() {
        let queue = FakeQueue::with_job();
        let work = FakeWork::taking(Duration::from_secs(50));

        run_leased_job(&queue, None, &work).await.unwrap();

        assert_eq!(
            queue.ledger().renewals,
            2,
            "due at 20s and 40s of a 50s run"
        );
    }
}
