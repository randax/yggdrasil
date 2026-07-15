use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use object_store::ObjectStore;
use yg_control::ControlPlane;

use crate::history::{extract_history, merge_history};
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
    /// `grace`, drop its control-plane row and delete its object-storage
    /// segments. A Shard that became current again between the scan and
    /// the delete is skipped; one whose row is deleted but object cleanup
    /// fails is logged as orphaned rather than blocking the rest. Returns
    /// how many Shards were collected.
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
        let Some(job) = self.control.claim_due_index(INDEX_LEASE).await? else {
            return Ok(false);
        };
        // A cold self-healing clone plus a long parse outlives the base
        // lease; the heartbeat keeps the job ours while the work is alive.
        let renew = async || self.control.renew_index(&job, INDEX_LEASE).await;
        let indexed = yg_sync::with_lease_heartbeat(INDEX_LEASE, renew, self.index(&job)).await;
        match indexed {
            Ok(shard) => {
                let applied = self
                    .control
                    .complete_index(
                        &job,
                        yg_control::ShardRecord {
                            revision: &shard.revision,
                            manifest_key: &shard.manifest_key,
                            commit_sha: &job.commit,
                            provenance_level: yg_shard::SYNTACTIC_PASS,
                            node_count: shard.node_count,
                            edge_count: shard.edge_count,
                        },
                    )
                    .await?;
                if applied {
                    tracing::info!(slug = %job.slug, revision = %shard.revision, "indexed");
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-index; result discarded");
                }
            }
            Err(e) => {
                let error = format!("{e:#}");
                if self.control.fail_index(&job, &error).await? {
                    tracing::warn!(slug = %job.slug, attempt = job.attempts + 1, error, "index failed");
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-index; failure discarded");
                }
            }
        }
        Ok(true)
    }

    /// Run the syntactic pass over the job's commit and publish the
    /// resulting Shard.
    async fn index(
        &self,
        job: &yg_control::LeasedIndex,
    ) -> anyhow::Result<yg_shard::PublishedShard> {
        // Revisions are deterministic per commit: when this one is
        // already published (a re-add, a retried job, another worker got
        // there first), answer from storage without touching git.
        if let Some(published) =
            yg_shard::published_shard(self.store.as_ref(), job.repo_id, &job.commit).await?
        {
            return Ok(published);
        }
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
            let mirror = self.ensure_mirror_has(job).await?;
            let commit = job.commit.clone();
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
            extract_history(&mirror, &commit).await?
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
        yg_shard::write_shard(
            self.store.as_ref(),
            job.repo_id,
            &job.commit,
            graph,
            search_docs,
        )
        .await
    }

    /// The local mirror, guaranteed to contain the job's commit. The
    /// cache is worker-local while the queue is not: a job can land on a
    /// host whose cache never saw the repo (or saw an older commit), so
    /// an indexing worker fetches the mirror itself when it must.
    async fn ensure_mirror_has(&self, job: &yg_control::LeasedIndex) -> anyhow::Result<PathBuf> {
        let mirror = yg_sync::mirror_path(&self.git_cache, job.repo_id);
        if commit_available(&mirror, &job.commit).await {
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
        if !commit_available(&mirror, &job.commit).await {
            anyhow::bail!(
                "commit {} is still missing after fetching {clone_url} — \
                 a shallow fetch depth that no longer reaches it, or \
                 rewritten history; re-adding the repo queues a fresh \
                 fetch whose newer commit supersedes this job",
                job.commit
            );
        }
        Ok(mirror)
    }
}

/// Whether `commit` is present in the (possibly absent) bare mirror.
/// `--git-dir`, not `-C`: discovery would climb out of a missing
/// mirror into whatever repository encloses the cache dir and answer
/// for the wrong repo.
async fn commit_available(mirror: &Path, commit: &str) -> bool {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir")
        .arg(mirror)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")]);
    cmd.kill_on_drop(true);
    matches!(cmd.status().await, Ok(status) if status.success())
}

/// Materialize `commit`'s tree from a bare mirror into `dest`, without
/// touching the mirror: `git archive` piped through `tar`.
fn extract_tree(mirror: &Path, commit: &str, dest: &Path) -> anyhow::Result<()> {
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
