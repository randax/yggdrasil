//! The lease lifecycle every job kind shares: claim, renew, release, fail.
//! Completion stays per-kind, because each kind's result differs.

use std::future::Future;
use std::time::Duration;

use crate::{ControlPlane, JobKind, LeaseToken, LeasedFetch, LeasedIndex};

/// A job claimed from the queue under a lease.
pub trait LeasedJob: Sized + Send + Sync {
    const KIND: JobKind;

    /// Claim the next due job of this kind; `None` when nothing is due.
    fn claim(
        control: &ControlPlane,
        lease: Duration,
    ) -> impl Future<Output = anyhow::Result<Option<Self>>> + Send;

    fn slug(&self) -> &str;

    /// Failures so far (0 on the first run).
    fn attempts(&self) -> i32;

    fn job_id(&self) -> i64;

    fn lease_token(&self) -> &LeaseToken;
}

impl LeasedJob for LeasedFetch {
    const KIND: JobKind = JobKind::Fetch;

    async fn claim(control: &ControlPlane, lease: Duration) -> anyhow::Result<Option<Self>> {
        control.claim_due_fetch(lease).await
    }

    fn slug(&self) -> &str {
        &self.slug
    }

    fn attempts(&self) -> i32 {
        self.attempts
    }

    fn job_id(&self) -> i64 {
        self.job_id
    }

    fn lease_token(&self) -> &LeaseToken {
        &self.lease_token
    }
}

impl LeasedJob for LeasedIndex {
    const KIND: JobKind = JobKind::Index;

    async fn claim(control: &ControlPlane, lease: Duration) -> anyhow::Result<Option<Self>> {
        control.claim_due_index(lease).await
    }

    fn slug(&self) -> &str {
        &self.slug
    }

    fn attempts(&self) -> i32 {
        self.attempts
    }

    fn job_id(&self) -> i64 {
        self.job_id
    }

    fn lease_token(&self) -> &LeaseToken {
        &self.lease_token
    }
}

impl ControlPlane {
    /// Extend a job's lease so work that outlives the base lease — a cold
    /// full-history clone — is not reclaimed mid-run. `false` means the job
    /// was reclaimed and this worker's token is dead, so it should abandon
    /// the run.
    pub async fn renew_job(&self, job: &impl LeasedJob, lease: Duration) -> anyhow::Result<bool> {
        self.renew_leased_job(job.job_id(), job.lease_token(), lease)
            .await
    }

    /// Return a claim to the immediately-due queue without recording a failed
    /// attempt. The lease token fences shutdown from releasing a job that a
    /// healthy worker has already reclaimed.
    pub async fn release_job(&self, job: &impl LeasedJob) -> anyhow::Result<bool> {
        self.release_leased_job(job.job_id(), job.lease_token())
            .await
    }

    /// Record a failed run: back the job off exponentially (30s doubling per
    /// attempt, capped at an hour) and keep the error for `yg admin status`.
    /// Jobs retry indefinitely — sync converges on its own once the cause
    /// (rate limit, token rotation) clears. `false` means the lease had lapsed
    /// and the job belongs to a newer claim.
    pub async fn fail_job(&self, job: &impl LeasedJob, error: &str) -> anyhow::Result<bool> {
        self.fail_leased_job(job.job_id(), job.lease_token(), error)
            .await
    }
}
