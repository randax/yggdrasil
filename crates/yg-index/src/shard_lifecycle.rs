//! Publishing and reclaiming Shard revisions. A revision's objects and its
//! control-plane row change together, under a per-revision fence, in an order
//! that keeps a pointer from ever reaching objects GC deleted. That ordering
//! lives here so callers cannot get it wrong.

use std::future::Future;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use futures::TryStreamExt;
use object_store::ObjectStore;
use object_store::path::Path;
use yg_control::{
    ControlPlane, LeasedIndex, ShardOperationFence, ShardOperationGuard, ShardRecord, ShardState,
    SupersededShard, finish_shard_operation,
};
use yg_shard::{PreparedShard, PublishedShard};

#[cfg(any(test, feature = "contract"))]
pub mod contract;
#[cfg(test)]
mod memory;

/// The revision fence and row state, all that orphan reclamation needs.
pub trait RevisionLocks: Sync {
    type Fence: ShardOperationFence;

    /// The fence, or `None` while another operation holds it.
    fn try_lock_revision(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> impl Future<Output = anyhow::Result<Option<Self::Fence>>> + Send;

    /// The revision's row state. Only authoritative while holding the fence.
    fn revision_state(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> impl Future<Output = anyhow::Result<Option<ShardState>>> + Send;
}

/// The control-plane half of every revision transition.
pub trait RevisionControl: RevisionLocks {
    type Job: Sync;

    fn lock_revision(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> impl Future<Output = anyhow::Result<Self::Fence>> + Send;

    /// Claim a row for reclamation. `false` means it became current again,
    /// so its objects must be left alone.
    fn begin_reclaiming(&self, shard_id: i64) -> impl Future<Output = anyhow::Result<bool>> + Send;

    /// Reap a claimed row once its objects are gone.
    fn finish_reclaiming(&self, shard_id: i64)
    -> impl Future<Output = anyhow::Result<bool>> + Send;

    /// Record the published Shard and swap the current pointer. `false` means
    /// the result was not applied: the lease lapsed, or the row is reclaiming.
    fn complete_publication(
        &self,
        job: &Self::Job,
        shard: ShardRecord<'_>,
    ) -> impl Future<Output = anyhow::Result<bool>> + Send;

    /// Return the job to the queue while its revision is being reclaimed.
    /// `false` means the lease lapsed.
    fn defer_publication(
        &self,
        job: &Self::Job,
    ) -> impl Future<Output = anyhow::Result<bool>> + Send;
}

impl RevisionLocks for ControlPlane {
    type Fence = ShardOperationGuard;

    async fn try_lock_revision(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<Option<ShardOperationGuard>> {
        self.try_lock_shard_operation(repo_id, revision).await
    }

    async fn revision_state(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<Option<ShardState>> {
        self.shard_state(repo_id, revision).await
    }
}

impl RevisionControl for ControlPlane {
    type Job = LeasedIndex;

    async fn lock_revision(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<ShardOperationGuard> {
        self.lock_shard_operation(repo_id, revision).await
    }

    async fn begin_reclaiming(&self, shard_id: i64) -> anyhow::Result<bool> {
        self.delete_superseded_shard(shard_id).await
    }

    async fn finish_reclaiming(&self, shard_id: i64) -> anyhow::Result<bool> {
        self.finish_shard_reclamation(shard_id).await
    }

    async fn complete_publication(
        &self,
        job: &LeasedIndex,
        shard: ShardRecord<'_>,
    ) -> anyhow::Result<bool> {
        self.complete_index(job, shard).await
    }

    async fn defer_publication(&self, job: &LeasedIndex) -> anyhow::Result<bool> {
        self.defer_index_for_reclamation(job).await
    }
}

/// Published objects awaiting [`settle_publication`]. Holds the revision fence,
/// so reclamation cannot delete those objects before the row records them.
pub struct Publication<Fence>(Attempt<Fence>);

enum Attempt<Fence> {
    Published {
        shard: PublishedShard,
        operation: Fence,
    },
    ReclamationInProgress {
        operation: Fence,
    },
}

/// How a [`Publication`] ended.
#[derive(Debug)]
pub enum PublicationSettlement {
    /// The row is recorded and the revision is `revision`.
    Applied { revision: String },
    /// The lease lapsed or the job's repo left indexing; retried when needed.
    NotApplied,
    /// The revision is being reclaimed; the job returned to the queue.
    Deferred,
    /// As `Deferred`, but the lease lapsed first.
    DeferralLostLease,
}

enum PrePublication<Fence> {
    Published {
        shard: PublishedShard,
        operation: Fence,
    },
    ReclamationInProgress {
        operation: Fence,
    },
    Prepared(PreparedShard),
}

async fn prepare_or_reuse_published<
    Fence,
    StateProbe,
    StateFuture,
    PublishedProbe,
    PublishedFuture,
    Lock,
    LockFuture,
    Prepare,
    PreparedFuture,
>(
    mut state: StateProbe,
    mut published: PublishedProbe,
    lock: Lock,
    prepare: Prepare,
) -> anyhow::Result<PrePublication<Fence>>
where
    Fence: ShardOperationFence,
    StateProbe: FnMut() -> StateFuture,
    StateFuture: Future<Output = anyhow::Result<Option<ShardState>>>,
    PublishedProbe: FnMut() -> PublishedFuture,
    PublishedFuture: Future<Output = anyhow::Result<Option<PublishedShard>>>,
    Lock: FnOnce() -> LockFuture,
    LockFuture: Future<Output = anyhow::Result<Fence>>,
    Prepare: FnOnce() -> PreparedFuture,
    PreparedFuture: Future<Output = anyhow::Result<PreparedShard>>,
{
    // A reclaiming row must take the normal preparation path. Its state may
    // change before preparation finishes, so only the later locked re-check
    // is allowed to decide whether the job is deferred.
    if state().await? == Some(ShardState::Reclaiming) {
        return Ok(PrePublication::Prepared(prepare().await?));
    }
    if published().await?.is_none() {
        return Ok(PrePublication::Prepared(prepare().await?));
    }

    let operation = lock().await?;
    if state().await? == Some(ShardState::Reclaiming) {
        return Ok(PrePublication::ReclamationInProgress { operation });
    }
    if let Some(shard) = published().await? {
        return Ok(PrePublication::Published { shard, operation });
    }

    // GC may have removed the artifact after the lock-free probe. Never parse
    // while holding the revision lock; release it and join the normal path.
    operation.release().await;
    Ok(PrePublication::Prepared(prepare().await?))
}

/// Write `commit`'s objects, or reuse them if already complete. `prepare` runs
/// outside the revision fence, because it can take minutes.
pub async fn publish_revision<C, P, F>(
    control: &C,
    store: &dyn ObjectStore,
    repo_id: i64,
    commit: &str,
    prepare: P,
) -> anyhow::Result<Publication<C::Fence>>
where
    C: RevisionControl,
    P: FnOnce() -> F,
    F: Future<Output = anyhow::Result<PreparedShard>>,
{
    let revision = yg_shard::syntactic_revision(commit);
    // This read-only probe is only an optimization. A complete current-schema
    // artifact lets retries avoid checkout, parsing, and holding prepared
    // segment bytes while they wait for the publication lock. Reclaiming
    // revisions deliberately miss this fastpath: the locked state check below
    // remains authoritative and decides deferral.
    let before_lock = prepare_or_reuse_published(
        || control.revision_state(repo_id, &revision),
        || yg_shard::published_shard(store, repo_id, commit),
        || async {
            control
                .lock_revision(repo_id, &revision)
                .await
                .context("locking the Shard revision for publication")
        },
        prepare,
    )
    .await?;
    let prepared = match before_lock {
        PrePublication::Published { shard, operation } => {
            return Ok(Publication(Attempt::Published { shard, operation }));
        }
        PrePublication::ReclamationInProgress { operation } => {
            return Ok(Publication(Attempt::ReclamationInProgress { operation }));
        }
        PrePublication::Prepared(prepared) => prepared,
    };
    // Only publication needs serialization: after taking the lock, re-check
    // reclamation and existing publication before the first object write.
    let operation = control
        .lock_revision(repo_id, &revision)
        .await
        .context("locking the Shard revision for publication")?;
    if control.revision_state(repo_id, &revision).await? == Some(ShardState::Reclaiming) {
        return Ok(Publication(Attempt::ReclamationInProgress { operation }));
    }
    // Revisions are deterministic per commit: a publisher that won while this
    // worker parsed lets us reuse its artifact without object writes.
    let shard = match yg_shard::published_shard(store, repo_id, commit).await? {
        Some(published) => published,
        None => yg_shard::publish_shard(store, repo_id, commit, prepared).await?,
    };
    Ok(Publication(Attempt::Published { shard, operation }))
}

/// Record a publication in the control plane, then release its fence.
pub async fn settle_publication<C: RevisionControl>(
    control: &C,
    job: &C::Job,
    commit: &str,
    publication: Publication<C::Fence>,
) -> anyhow::Result<PublicationSettlement> {
    match publication.0 {
        Attempt::Published { shard, operation } => {
            let applied = finish_shard_operation(
                operation,
                control.complete_publication(
                    job,
                    ShardRecord {
                        revision: &shard.revision,
                        manifest_key: &shard.manifest_key,
                        commit_sha: commit,
                        provenance_level: yg_shard::SYNTACTIC_PASS,
                        node_count: shard.node_count,
                        edge_count: shard.edge_count,
                    },
                ),
            )
            .await?;
            Ok(if applied {
                PublicationSettlement::Applied {
                    revision: shard.revision,
                }
            } else {
                PublicationSettlement::NotApplied
            })
        }
        Attempt::ReclamationInProgress { operation } => {
            let deferred =
                finish_shard_operation(operation, control.defer_publication(job)).await?;
            Ok(if deferred {
                PublicationSettlement::Deferred
            } else {
                PublicationSettlement::DeferralLostLease
            })
        }
    }
}

/// Reclaim a superseded revision: claim its row, delete its objects, reap the
/// row. Returns whether it was reclaimed. A failed delete leaves the row
/// reclaiming, which fences publication until a later sweep resumes it.
pub async fn reclaim_superseded<C: RevisionControl>(
    control: &C,
    store: &dyn ObjectStore,
    shard: &SupersededShard,
) -> anyhow::Result<bool> {
    let operation = control
        .lock_revision(shard.repo_id, &shard.revision)
        .await
        .context("locking the Shard revision for reclamation")?;
    finish_shard_operation(operation, async {
        if !control.begin_reclaiming(shard.shard_id).await? {
            return Ok(false);
        }
        yg_shard::delete_shard(store, shard.repo_id, &shard.revision)
            .await
            .context("deleting the Shard's object-storage segments")?;
        control
            .finish_reclaiming(shard.shard_id)
            .await
            .context("reaping the reclaimed Shard row")
    })
    .await
}

/// Delete a revision's objects if it has no row and nothing under `prefix` is
/// younger than `grace`. Returns whether they were deleted. Skips a revision
/// whose fence is held rather than waiting on it.
pub async fn reclaim_orphan<C: RevisionLocks>(
    control: &C,
    store: &dyn ObjectStore,
    repo_id: i64,
    revision: &str,
    prefix: &Path,
    now: SystemTime,
    grace: Duration,
) -> anyhow::Result<bool> {
    // Unlocked fast path: most examined revisions are healthy, and a present
    // row means "live, skip" — a decision that needs no lock and saves the
    // per-revision prefix listing. The authoritative row check still happens
    // under the lock before any deletion.
    if control.revision_state(repo_id, revision).await?.is_some() {
        return Ok(false);
    }
    if !prefix_is_past_grace(store, prefix, now, grace).await? {
        return Ok(false);
    }
    let Some(operation) = control
        .try_lock_revision(repo_id, revision)
        .await
        .with_context(|| format!("trying to lock orphan candidate {prefix}"))?
    else {
        return Ok(false);
    };
    finish_shard_operation(operation, async {
        // Re-list under the lock: the preliminary age check is only an
        // optimization, and a publisher may have written another object
        // before its operation lock became visible to this sweep.
        if !prefix_is_past_grace(store, prefix, now, grace).await? {
            return Ok(false);
        }
        if control.revision_state(repo_id, revision).await?.is_some() {
            return Ok(false);
        }
        yg_shard::delete_shard(store, repo_id, revision)
            .await
            .with_context(|| format!("deleting orphaned Shard prefix {prefix}"))?;
        Ok(true)
    })
    .await
}

async fn prefix_is_past_grace(
    store: &dyn ObjectStore,
    prefix: &Path,
    now: SystemTime,
    grace: Duration,
) -> anyhow::Result<bool> {
    let newest = store
        .list(Some(prefix))
        .map_ok(|object| SystemTime::from(object.last_modified))
        .try_fold(None, |newest, modified| async move {
            Ok(Some(newest.map_or(modified, |current: SystemTime| {
                current.max(modified)
            })))
        })
        .await?;
    Ok(newest.is_some_and(|modified| now.duration_since(modified).is_ok_and(|age| age >= grace)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct CountingFence<'a> {
        releases: &'a AtomicUsize,
    }

    impl ShardOperationFence for CountingFence<'_> {
        async fn release(self) {
            self.releases.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn an_already_published_revision_rechecks_under_lock_without_preparation() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let repo_id = 17;
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let fixture = yg_shard::prepare_shard(yg_shard::Graph::default(), Vec::new())
            .await
            .expect("fixture segments build");
        yg_shard::publish_shard(store.as_ref(), repo_id, commit, fixture)
            .await
            .expect("fixture shard publishes");

        let state_probes = AtomicUsize::new(0);
        let publication_probes = AtomicUsize::new(0);
        let lock_acquisitions = AtomicUsize::new(0);
        let lock_releases = AtomicUsize::new(0);
        let preparation_runs = AtomicUsize::new(0);
        let result = prepare_or_reuse_published(
            || {
                state_probes.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(Some(ShardState::Published)))
            },
            || {
                publication_probes.fetch_add(1, Ordering::SeqCst);
                let store = store.clone();
                async move { yg_shard::published_shard(store.as_ref(), repo_id, commit).await }
            },
            || {
                lock_acquisitions.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(CountingFence {
                    releases: &lock_releases,
                }))
            },
            || async {
                preparation_runs.fetch_add(1, Ordering::SeqCst);
                yg_shard::prepare_shard(yg_shard::Graph::default(), Vec::new()).await
            },
        )
        .await
        .expect("the published fastpath succeeds");

        let PrePublication::Published { shard, operation } = result else {
            panic!("the current published shard must take the fastpath");
        };
        assert_eq!(shard.revision, yg_shard::syntactic_revision(commit));
        assert_eq!(state_probes.load(Ordering::SeqCst), 2);
        assert_eq!(publication_probes.load(Ordering::SeqCst), 2);
        assert_eq!(lock_acquisitions.load(Ordering::SeqCst), 1);
        assert_eq!(preparation_runs.load(Ordering::SeqCst), 0);
        assert_eq!(lock_releases.load(Ordering::SeqCst), 0);
        operation.release().await;
        assert_eq!(lock_releases.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_reclaiming_revision_bypasses_the_published_fastpath() {
        let publication_probes = AtomicUsize::new(0);
        let lock_acquisitions = AtomicUsize::new(0);
        let lock_releases = AtomicUsize::new(0);
        let preparation_runs = AtomicUsize::new(0);
        let result = prepare_or_reuse_published(
            || std::future::ready(Ok(Some(ShardState::Reclaiming))),
            || {
                publication_probes.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(None::<PublishedShard>))
            },
            || {
                lock_acquisitions.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(CountingFence {
                    releases: &lock_releases,
                }))
            },
            || async {
                preparation_runs.fetch_add(1, Ordering::SeqCst);
                yg_shard::prepare_shard(yg_shard::Graph::default(), Vec::new()).await
            },
        )
        .await
        .expect("reclaiming joins the normal preparation path");

        assert!(matches!(result, PrePublication::Prepared(_)));
        assert_eq!(publication_probes.load(Ordering::SeqCst), 0);
        assert_eq!(lock_acquisitions.load(Ordering::SeqCst), 0);
        assert_eq!(preparation_runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn every_adapter_case_holds_for_the_in_memory_adapter() {
        contract::verify(async || memory::MemoryHarness::new()).await;
    }
}
