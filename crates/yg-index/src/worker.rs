use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use object_store::ObjectStore;
use yg_control::ControlPlane;

use crate::commit::CommitSha;
use crate::history::{extract_history_for_commit, merge_history};
use crate::pass::syntactic_pass;

/// How long a worker may hold an index job before a crashed run becomes
/// claimable again. Budgeted for the worst case the path actually does:
/// self-healing the mirror — a cold full-history clone, the whole of
/// FETCH_LEASE's budget — plus checkout extraction plus the parse
/// (minutes, RFC 0001 §4). An overrun loses no result — write_shard
/// publishes before the fence, so a re-claim answers from storage — but
/// it does buy a duplicate cold clone on another host.
const INDEX_LEASE: Duration = Duration::from_secs(30 * 60);

/// An indexing worker: drains the index queue, running the syntactic
/// pass over synced checkouts and publishing Shards.
pub struct IndexWorker {
    control: ControlPlane,
    store: Arc<dyn ObjectStore>,
    git_cache: PathBuf,
}

enum IndexAttempt {
    Published {
        shard: yg_shard::PublishedShard,
        operation: yg_control::ShardOperationGuard,
    },
    ReclamationInProgress {
        operation: yg_control::ShardOperationGuard,
    },
}

enum PrePublication<Fence> {
    Published {
        shard: yg_shard::PublishedShard,
        operation: Fence,
    },
    ReclamationInProgress {
        operation: Fence,
    },
    Prepared(yg_shard::PreparedShard),
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
    Fence: yg_control::ShardOperationFence,
    StateProbe: FnMut() -> StateFuture,
    StateFuture: Future<Output = anyhow::Result<Option<yg_control::ShardState>>>,
    PublishedProbe: FnMut() -> PublishedFuture,
    PublishedFuture: Future<Output = anyhow::Result<Option<yg_shard::PublishedShard>>>,
    Lock: FnOnce() -> LockFuture,
    LockFuture: Future<Output = anyhow::Result<Fence>>,
    Prepare: FnOnce() -> PreparedFuture,
    PreparedFuture: Future<Output = anyhow::Result<yg_shard::PreparedShard>>,
{
    // A reclaiming row must take the normal preparation path. Its state may
    // change before preparation finishes, so only the later locked re-check
    // is allowed to decide whether the job is deferred.
    if state().await? == Some(yg_control::ShardState::Reclaiming) {
        return Ok(PrePublication::Prepared(prepare().await?));
    }
    if published().await?.is_none() {
        return Ok(PrePublication::Prepared(prepare().await?));
    }

    let operation = lock().await?;
    if state().await? == Some(yg_control::ShardState::Reclaiming) {
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

impl IndexWorker {
    pub fn new(
        control: ControlPlane,
        store: Arc<dyn ObjectStore>,
        git_cache: impl Into<PathBuf>,
    ) -> Self {
        Self {
            control,
            store,
            git_cache: git_cache.into(),
        }
    }

    /// Queue a re-index for every repo whose current Shard predates this
    /// binary's index schema — a deploy bumped [`yg_shard::SCHEMA_VERSION`]
    /// and the read path now refuses the older artifacts. Run once at
    /// worker boot: re-indexing republishes at the current (deterministic)
    /// revision and swaps the pointer, so the fleet self-heals. Returns
    /// how many repos were queued.
    pub async fn requeue_outdated_shards(&self) -> anyhow::Result<u64> {
        let queued = self
            .control
            .requeue_outdated_shards(&yg_shard::syntactic_revision_suffix())
            .await?;
        if queued > 0 {
            tracing::info!(
                repos = queued,
                "queued re-index for Shards from an older schema"
            );
        }
        Ok(queued)
    }

    /// Reclaim object storage from superseded Shards (issue #9): for every
    /// Shard no repo points at that has been superseded longer than
    /// `grace`, claim its row, delete its object-storage segments, and
    /// reap the row. A Shard that became current again between the scan
    /// and claim is skipped; failed cleanup leaves a reclaiming row for
    /// the next sweep to resume. Returns how many Shards were collected.
    pub async fn gc_once(&self, grace: Duration) -> anyhow::Result<u64> {
        crate::gc::collect_superseded(&self.control, self.store.as_ref(), grace).await
    }

    /// Remove terminal job rows finished longer ago than `retention`
    /// (issue #49): nothing reads a job row after it settles — even
    /// `yg admin status` excludes terminal rows — so retention exists
    /// purely to stop the queue table growing forever. Runs beside
    /// [`Self::gc_once`] on the GC cadence. Returns how many rows were
    /// removed.
    pub async fn retire_terminal_jobs(&self, retention: Duration) -> anyhow::Result<u64> {
        crate::gc::retire_terminal_jobs(&self.control, retention).await
    }

    /// Claim and run one due index job. Returns whether there was work.
    /// A failed run is recorded (with backoff) rather than returned as an
    /// error — `Err` means the control plane itself is unreachable.
    pub async fn run_once(&self) -> anyhow::Result<bool> {
        self.run_once_with_optional_shutdown(None).await
    }

    /// Claim and run one due job while observing process shutdown. New
    /// claims stop immediately; an active index gets until the shared
    /// work cutoff to settle normally, then its lease is returned fresh
    /// to the queue before the work future is dropped.
    pub async fn run_once_with_shutdown(
        &self,
        shutdown: yg_sync::Shutdown,
    ) -> anyhow::Result<bool> {
        if shutdown.deadline().is_some() {
            return Ok(false);
        }
        self.run_once_with_optional_shutdown(Some(shutdown)).await
    }

    async fn run_once_with_optional_shutdown(
        &self,
        shutdown: Option<yg_sync::Shutdown>,
    ) -> anyhow::Result<bool> {
        let (job, timer) = match claim_due_index_with_optional_shutdown(
            shutdown.as_ref(),
            self.control.claim_due_index(INDEX_LEASE),
            async |job| self.control.release_index(job).await,
            || self.control.start_job(yg_control::JobKind::Index),
        )
        .await?
        {
            ShutdownClaim::Empty => return Ok(false),
            ShutdownClaim::Ready { job, timer } => (job, timer),
            ShutdownClaim::Released { timer } => {
                // The job was released untouched for a healthy retry: no
                // work happened, so no outcome is recorded.
                timer.disarm();
                return Ok(true);
            }
        };
        // A cold self-healing clone plus a long parse outlives the base
        // lease; the heartbeat keeps the job ours while the work is alive.
        let renew = async || self.control.renew_index(&job, INDEX_LEASE).await;
        let indexed = if let Some(shutdown) = shutdown {
            let release = async || self.control.release_index(&job).await;
            match yg_sync::with_lease_heartbeat_until_shutdown(
                INDEX_LEASE,
                renew,
                release,
                shutdown,
                self.index(&job),
            )
            .await?
            {
                yg_sync::LeaseShutdown::Finished(indexed) => indexed,
                yg_sync::LeaseShutdown::Released => {
                    timer.finish(yg_control::JobOutcome::Discarded);
                    return Ok(true);
                }
            }
        } else {
            yg_sync::with_lease_heartbeat(INDEX_LEASE, renew, self.index(&job)).await
        };
        match indexed {
            Ok(IndexAttempt::Published { shard, operation }) => {
                let applied = yg_control::finish_shard_operation(
                    operation,
                    self.control.complete_index(
                        &job,
                        yg_control::ShardRecord {
                            revision: &shard.revision,
                            manifest_key: &shard.manifest_key,
                            commit_sha: &job.commit,
                            provenance_level: yg_shard::SYNTACTIC_PASS,
                            node_count: shard.node_count,
                            edge_count: shard.edge_count,
                        },
                    ),
                )
                .await?;
                if applied {
                    tracing::info!(slug = %job.slug, revision = %shard.revision, "indexed");
                    timer.finish(yg_control::JobOutcome::Success);
                } else {
                    tracing::warn!(slug = %job.slug, "index result was fenced or its lease lapsed; job will retry when needed");
                    timer.finish(yg_control::JobOutcome::Discarded);
                }
            }
            Ok(IndexAttempt::ReclamationInProgress { operation }) => {
                let deferred = yg_control::finish_shard_operation(
                    operation,
                    self.control.defer_index_for_reclamation(&job),
                )
                .await?;
                if deferred {
                    tracing::info!(slug = %job.slug, "deferred index while its Shard revision is being reclaimed");
                } else {
                    tracing::warn!(slug = %job.slug, "index reclamation deferral lost its lease");
                }
                timer.finish(yg_control::JobOutcome::Discarded);
            }
            Err(e) => {
                let error = format!("{e:#}");
                if self.control.fail_index(&job, &error).await? {
                    tracing::warn!(slug = %job.slug, attempt = job.attempts + 1, error, "index failed");
                    timer.finish(yg_control::JobOutcome::Failure);
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-index; failure discarded");
                    timer.finish(yg_control::JobOutcome::Discarded);
                }
            }
        }
        Ok(true)
    }

    /// Run the syntactic pass over the job's commit and publish the
    /// resulting Shard.
    async fn index(&self, job: &yg_control::LeasedIndex) -> anyhow::Result<IndexAttempt> {
        let commit = commit_for_job(&job.commit)?;
        let revision = yg_shard::syntactic_revision(commit.as_str());
        // This read-only probe is only an optimization. A complete current-
        // schema artifact lets retries avoid checkout, parsing, and holding
        // prepared segment bytes while they wait for the publication lock.
        // Reclaiming revisions deliberately miss this fastpath: the locked
        // state check below remains authoritative and decides deferral.
        let before_lock = prepare_or_reuse_published(
            || self.control.shard_state(job.repo_id, &revision),
            || yg_shard::published_shard(self.store.as_ref(), job.repo_id, commit.as_str()),
            || async {
                self.control
                    .lock_shard_operation(job.repo_id, &revision)
                    .await
                    .context("locking the Shard revision for publication")
            },
            || self.prepare(job, &commit),
        )
        .await?;
        let prepared = match before_lock {
            PrePublication::Published { shard, operation } => {
                return Ok(IndexAttempt::Published { shard, operation });
            }
            PrePublication::ReclamationInProgress { operation } => {
                return Ok(IndexAttempt::ReclamationInProgress { operation });
            }
            PrePublication::Prepared(prepared) => prepared,
        };
        // Parsing and segment construction are deliberately outside the
        // Shard-operation lock. Only publication needs serialization: after
        // taking the lock, re-check reclamation and existing publication
        // before the first object write.
        let operation = self
            .control
            .lock_shard_operation(job.repo_id, &revision)
            .await
            .context("locking the Shard revision for publication")?;
        if self.control.shard_state(job.repo_id, &revision).await?
            == Some(yg_control::ShardState::Reclaiming)
        {
            return Ok(IndexAttempt::ReclamationInProgress { operation });
        }
        // Revisions are deterministic per commit: a publisher that won while
        // this worker parsed lets us reuse its artifact without object writes.
        if let Some(published) =
            yg_shard::published_shard(self.store.as_ref(), job.repo_id, commit.as_str()).await?
        {
            return Ok(IndexAttempt::Published {
                shard: published,
                operation,
            });
        }
        let shard =
            yg_shard::publish_shard(self.store.as_ref(), job.repo_id, commit.as_str(), prepared)
                .await?;
        Ok(IndexAttempt::Published { shard, operation })
    }

    async fn prepare(
        &self,
        job: &yg_control::LeasedIndex,
        commit: &CommitSha,
    ) -> anyhow::Result<yg_shard::PreparedShard> {
        let checkout = tempfile::tempdir().context("creating a scratch checkout dir")?;
        // Materialize the checkout and walk the git history while the
        // mirror is locked: both read mirror objects, so a concurrent
        // fetch reshaping the mirror — the availability check self-healing,
        // a `--prune` GC'ing objects — could yank the commit (or its
        // ancestors) out from under `git archive` or `git log`. The lock is
        // released before the parse: it reads only the private checkout,
        // and holding the repo's lock through minutes of parsing would
        // starve its fetches.
        let history = {
            let _serialize_same_mirror =
                yg_sync::lock_mirror(&self.git_cache, job.repo_id, INDEX_LEASE).await?;
            let mirror = self.ensure_mirror_has(job, commit).await?;
            // The checkout extraction is blocking (archive | tar); run it
            // off the runtime threads, under the lock.
            {
                let mirror = mirror.clone();
                let commit = commit.clone();
                let dest = checkout.path().to_path_buf();
                tokio::task::spawn_blocking(move || extract_tree(&mirror, &commit, &dest))
                    .await
                    .context("checkout extraction task panicked")??;
            }
            // The history walk runs its own (timeout- and kill-guarded) git
            // log, still under the mirror lock so a concurrent prune can't
            // GC objects mid-walk.
            extract_history_for_commit(&mirror, commit).await?
        };
        let (mut graph, search_docs) = {
            let dest = checkout.path().to_path_buf();
            tokio::task::spawn_blocking(move || syntactic_pass(&dest))
                .await
                .context("syntactic pass task panicked")??
        };
        // Fold history in once the syntactic File nodes exist — they are
        // the ground truth for which TOUCHES edges may keep their target.
        merge_history(&mut graph, history);
        yg_shard::prepare_shard(graph, search_docs).await
    }

    /// The local mirror, guaranteed to contain the job's commit. The
    /// cache is worker-local while the queue is not: a job can land on a
    /// host whose cache never saw the repo (or saw an older commit), so
    /// an indexing worker fetches the mirror itself when it must.
    async fn ensure_mirror_has(
        &self,
        job: &yg_control::LeasedIndex,
        commit: &CommitSha,
    ) -> anyhow::Result<PathBuf> {
        let mirror = yg_sync::mirror_path(&self.git_cache, job.repo_id);
        if commit_available(&mirror, commit).await {
            return Ok(mirror);
        }
        let clone_url = yg_sync::join_clone_url(&job.base_url, &job.slug);
        tracing::info!(slug = %job.slug, "local mirror lacks the commit; fetching");
        let forge = yg_sync::forge::builtin().for_kind(&job.forge_kind);
        let auth = yg_sync::forge_token(job.token_env.as_deref(), &clone_url)
            .map(|token| forge.git_auth(token));
        yg_sync::GitFetcher::new(&self.git_cache)
            .sync(job.repo_id, &clone_url, auth.as_ref(), job.fetch_depth)
            .await
            .context("fetching the mirror for indexing")?;
        // A fetch brings the remote's *current* state, which is not
        // necessarily the job's commit: a shallow depth override prunes
        // older commits, and rewritten history drops them entirely. Say
        // so instead of letting git archive fail cryptically downstream.
        if !commit_available(&mirror, commit).await {
            anyhow::bail!(
                "commit {} is still missing after fetching {clone_url} — \
                 a shallow fetch depth that no longer reaches it, or \
                 rewritten history; re-adding the repo queues a fresh \
                 fetch whose newer commit supersedes this job",
                commit
            );
        }
        Ok(mirror)
    }
}

enum ShutdownClaim<T, M> {
    Empty,
    Ready { job: T, timer: M },
    Released { timer: M },
}

async fn claim_due_index_with_optional_shutdown<T, M>(
    shutdown: Option<&yg_sync::Shutdown>,
    claim: impl Future<Output = anyhow::Result<Option<T>>>,
    release: impl AsyncFnOnce(&T) -> anyhow::Result<bool>,
    start_timer: impl FnOnce() -> M,
) -> anyhow::Result<ShutdownClaim<T, M>> {
    let Some(job) = claim.await? else {
        return Ok(ShutdownClaim::Empty);
    };
    let timer = start_timer();
    if shutdown.is_some_and(|shutdown| shutdown.request().is_some()) {
        let released = release(&job).await?;
        tracing::info!(released, "released fresh index claim for shutdown");
        return Ok(ShutdownClaim::Released { timer });
    }
    Ok(ShutdownClaim::Ready { job, timer })
}

/// Parse the untyped control-plane payload before the worker performs storage,
/// filesystem, network, or subprocess work. Every git helper below requires
/// the resulting type, so an invalid queue value cannot reach `git`.
fn commit_for_job(commit: &str) -> anyhow::Result<CommitSha> {
    Ok(CommitSha::try_from(commit)?)
}

/// Whether `commit` is present in the (possibly absent) bare mirror.
/// `--git-dir`, not `-C`: discovery would climb out of a missing
/// mirror into whatever repository encloses the cache dir and answer
/// for the wrong repo.
async fn commit_available(mirror: &Path, commit: &CommitSha) -> bool {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir")
        .arg(mirror)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")]);
    cmd.kill_on_drop(true);
    matches!(cmd.status().await, Ok(status) if status.success())
}

/// Materialize `commit`'s tree from a bare mirror into `dest`, without
/// touching the mirror: `git archive` piped through `tar`.
fn extract_tree(mirror: &Path, commit: &CommitSha, dest: &Path) -> anyhow::Result<()> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut archive = Command::new("git")
        // --git-dir, not -C: never let repo discovery climb out of the
        // mirror path into an enclosing repository.
        .arg("--git-dir")
        .arg(mirror)
        // The ^{tree} suffix matters: archiving a commit embeds a pax
        // global header carrying the commit id, which some tar
        // implementations (busybox) extract as a phantom file — the same
        // revision would then index differently across worker images.
        .args(["archive", &format!("{commit}^{{tree}}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .spawn()
        .context("running git archive (is git installed on this worker?)")?;
    // Drain stderr concurrently: left unread, a chatty git (GIT_TRACE,
    // attribute warnings) fills the pipe buffer and deadlocks the
    // archive | tar pipeline.
    let mut archive_stderr = archive.stderr.take().expect("stderr was piped above");
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = archive_stderr.read_to_string(&mut buf);
        buf
    });
    let unpack = Command::new("tar")
        // -f - pins the archive to stdin: without it, an inherited TAPE
        // env var (or an odd compiled-in default) makes tar ignore the
        // pipe entirely.
        .args(["-x", "-f", "-"])
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::from(
            archive.stdout.take().expect("stdout was piped above"),
        ))
        .stderr(Stdio::piped())
        .output();
    let unpack = match unpack {
        Ok(output) => output,
        // tar never spawned (missing binary, fork pressure): reap git
        // before bailing — a std Child is neither killed nor reaped on
        // drop, and this path repeats every retry, accruing one zombie
        // per attempt for the worker's lifetime.
        Err(e) => {
            let _ = archive.kill();
            let _ = archive.wait();
            let _ = stderr_reader.join();
            return Err(e).context("running tar (is tar installed on this worker?)");
        }
    };
    let archive_status = archive.wait().context("waiting for git archive")?;
    let archive_stderr = stderr_reader
        .join()
        .unwrap_or_else(|_| "stderr reader panicked".to_string());
    // tar's verdict first: when tar dies (disk full, unwritable dest),
    // git takes an EPIPE and "fails" with nothing on stderr — blaming
    // git would hide the actual cause.
    if !unpack.status.success() {
        let tar_stderr = String::from_utf8_lossy(&unpack.stderr);
        let git_said = if archive_status.success() || archive_stderr.trim().is_empty() {
            String::new()
        } else {
            format!(" (git archive: {})", archive_stderr.trim())
        };
        anyhow::bail!(
            "unpacking the checkout failed: {}{git_said}",
            tar_stderr.trim()
        );
    }
    if !archive_status.success() {
        anyhow::bail!("git archive {commit} failed: {}", archive_stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use tokio::time::Instant;
    use yg_control::ShardOperationFence;

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
                std::future::ready(Ok(Some(yg_control::ShardState::Published)))
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
            || std::future::ready(Ok(Some(yg_control::ShardState::Reclaiming))),
            || {
                publication_probes.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(None::<yg_shard::PublishedShard>))
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
    async fn index_claim_completing_after_shutdown_is_released_before_work_starts() {
        let (trigger, shutdown) = yg_sync::shutdown_channel();
        let released_job = AtomicUsize::new(0);
        let claim = async {
            assert!(trigger.request(
                Instant::now() + Duration::from_secs(30),
                yg_sync::ShutdownCause::Signal,
            ));
            Ok(Some(43_usize))
        };

        let claimed = claim_due_index_with_optional_shutdown(
            Some(&shutdown),
            claim,
            async |job| {
                released_job.store(*job, Ordering::SeqCst);
                Ok(true)
            },
            || 79_usize,
        )
        .await
        .expect("post-claim shutdown check");

        assert!(matches!(claimed, ShutdownClaim::Released { timer: 79 }));
        assert_eq!(released_job.load(Ordering::SeqCst), 43);
    }

    #[test]
    fn malformed_commit_is_rejected_at_worker_boundary_before_git_can_spawn() {
        let error = commit_for_job("--exec=x").expect_err("option-like commit must be rejected");

        assert_eq!(error.to_string(), "invalid commit sha \"--exec=x\"");
    }
}
