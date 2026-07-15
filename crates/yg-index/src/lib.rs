//! Tree-sitter pass, SCIP ingestion, extractors, sandbox driver.
//!
//! M0 ships the syntactic pass (RFC 0001 §4, ADR 0002): tree-sitter over
//! a synced checkout, Go grammar for Symbols and DEFINES edges plus
//! heuristic CALLS / IMPORTS / EXTENDS / IMPLEMENTS edges resolved by
//! name and scope (ambiguity policy: ADR 0006), every other file a File
//! node. The precise SCIP pass arrives with M1.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use object_store::ObjectStore;
use yg_control::ControlPlane;
use yg_shard::{Edge, EdgeKind, Graph, Node, NodeKind, Provenance, SearchDoc};

/// Cap on the text indexed per file (RFC 0001 §6 full-text segment): a
/// giant generated or vendored blob must not bloat the segment. Past this,
/// the file is searchable by name; its content is truncated on a char
/// boundary.
const MAX_BODY_BYTES: usize = 512 * 1024;

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
        let stale = self.control.superseded_shards_past_grace(grace).await?;
        let mut collected = 0;
        for shard in &stale {
            match self.collect_shard(shard).await {
                Ok(true) => collected += 1,
                // Became current again between the scan and now — left be.
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    repo_id = shard.repo_id,
                    revision = %shard.revision,
                    error = format!("{e:#}"),
                    "could not finish reclaiming superseded Shard; a later sweep will retry"
                ),
            }
        }
        if collected > 0 {
            tracing::info!(shards = collected, "garbage-collected superseded Shards");
        }
        Ok(collected)
    }

    /// Remove terminal job rows finished longer ago than `retention`
    /// (issue #49): nothing reads a job row after it settles — even
    /// `yg admin status` excludes terminal rows — so retention exists
    /// purely to stop the queue table growing forever. Runs beside
    /// [`Self::gc_once`] on the GC cadence. Returns how many rows were
    /// removed.
    pub async fn retire_terminal_jobs(&self, retention: Duration) -> anyhow::Result<u64> {
        let deleted = self
            .control
            .delete_terminal_jobs_past_retention(retention)
            .await?;
        if deleted > 0 {
            tracing::info!(jobs = deleted, "removed terminal jobs past retention");
        }
        Ok(deleted)
    }

    /// Reclaim one superseded Shard. The control-plane row first moves to
    /// `reclaiming`, preserving the `(repo, revision)` uniqueness fence
    /// while object deletion runs. A colliding index completion requeues
    /// instead of pointing at those objects. Successful deletion reaps
    /// the row; a crash leaves it reclaiming for the next sweep. Returns
    /// whether the Shard was collected (`false` = it became current again
    /// and was skipped).
    async fn collect_shard(&self, shard: &yg_control::SupersededShard) -> anyhow::Result<bool> {
        let operation = self
            .control
            .lock_shard_operation(shard.repo_id, &shard.revision)
            .await
            .context("locking the Shard revision for reclamation")?;
        yg_control::finish_shard_operation(operation, async {
            if !self.control.delete_superseded_shard(shard.shard_id).await? {
                return Ok(false);
            }
            yg_shard::delete_shard(self.store.as_ref(), shard.repo_id, &shard.revision)
                .await
                .context("deleting the Shard's object-storage segments")?;
            self.control
                .finish_shard_reclamation(shard.shard_id)
                .await
                .context("reaping the reclaimed Shard row")
        })
        .await
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
                } else {
                    tracing::warn!(slug = %job.slug, "index result was fenced or its lease lapsed; job will retry when needed");
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
    async fn index(&self, job: &yg_control::LeasedIndex) -> anyhow::Result<IndexAttempt> {
        let revision = yg_shard::syntactic_revision(&job.commit);
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
        let prepared = yg_shard::prepare_shard(graph, search_docs).await?;
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
            yg_shard::published_shard(self.store.as_ref(), job.repo_id, &job.commit).await?
        {
            return Ok(IndexAttempt::Published {
                shard: published,
                operation,
            });
        }
        let shard =
            yg_shard::publish_shard(self.store.as_ref(), job.repo_id, &job.commit, prepared)
                .await?;
        Ok(IndexAttempt::Published { shard, operation })
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

/// The git-history layer (RFC 0001 §4): walk every commit reachable from
/// `commit` and distill it into Commit and Contributor nodes plus AUTHORED
/// (Contributor→Commit) and TOUCHES (Commit→File) edges. Contributors are
/// deduped by email within this Shard — cross-Forge identity merge is M2.
///
/// A TOUCHES edge is emitted for *every* path a commit changed, named as
/// the File node it would have (`file:<path>`). The history walk can't see
/// which of those Files the syntactic pass minted nodes for (a since-deleted
/// path, a non-UTF-8 name it skipped), so `merge_history` prunes TOUCHES
/// to absent Files when folding this into the syntactic graph — the read
/// path refuses a graph with edges to nodes it doesn't hold. Commits whose
/// every changed path is gone still become Commit nodes (full history is
/// ingested), exactly as `git log -- <deleted-path>` omits them from a live
/// file's history.
///
/// Every edge is `extracted` provenance at confidence 1.0 (CONTEXT.md):
/// git history is deterministically derived, not heuristically guessed.
///
/// Parsing is NUL-delimited (`git log -z`): commit messages, names, and
/// emails are attacker-controlled by anyone who can push, and NUL is the
/// one byte git forbids in a message or pathname — so a crafted subject
/// can't forge a record, shift a field, or corrupt a path. As a second
/// guard a record whose first field isn't a hex sha is skipped.
///
/// Reads the bare mirror but never mutates it — the caller holds the
/// mirror lock, since a concurrent `--prune` fetch could GC objects
/// mid-walk. `--git-dir`, never `-C`: repository discovery must not climb
/// out of a missing mirror into an enclosing checkout.
pub async fn extract_history(git_dir: &Path, commit: &str) -> anyhow::Result<Graph> {
    let log = git_log_history(git_dir, commit).await?;
    // Walk the log off the runtime threads: every other CPU-bound pass
    // (syntactic_pass, extract_tree, the segment builds in write_shard)
    // runs in spawn_blocking, and a large repo's history is non-trivial to
    // parse — keep it off the async executor.
    tokio::task::spawn_blocking(move || parse_history(&log))
        .await
        .context("history parse task panicked")?
}

/// Parse the NUL-framed `git log -z` output into a history graph. Split
/// from [`extract_history`] so the CPU-bound walk can run in spawn_blocking.
///
/// The `-z` stream is NUL-separated tokens: each record is the five header
/// fields (`%H %ct %an %ae %s`, each NUL-terminated) followed by the
/// `--name-only` paths (NUL-terminated), then a NUL record separator (an
/// empty token). Only the first path of a record carries git's leading
/// `\n` separator between the format and the file list. Iterating the
/// split directly avoids materializing one big Vec of token slices.
fn parse_history(log: &str) -> anyhow::Result<Graph> {
    let mut graph = Graph::default();
    // Emit each contributor's node once; AUTHORED edges still attribute
    // every commit, deduping only the node by email.
    let mut seen_contributors: HashSet<String> = HashSet::new();
    let mut tokens = log.split('\0');
    while let Some(sha) = tokens.next() {
        // Skip record separators and the trailing empty token.
        if sha.is_empty() {
            continue;
        }
        // The rest of the five-field header; a short tail is truncation.
        let (Some(ct), Some(name), Some(email), Some(subject)) =
            (tokens.next(), tokens.next(), tokens.next(), tokens.next())
        else {
            break;
        };
        // Paths run until the record-separator (empty) token or the end;
        // the `for` consumes that separator so the next record starts clean.
        let mut paths: Vec<&str> = Vec::new();
        let mut first_path = true;
        for token in tokens.by_ref() {
            if token.is_empty() {
                break;
            }
            // git inserts a `\n` between the format and the file list; it
            // rides on the first path token only.
            let path = if first_path {
                token.strip_prefix('\n').unwrap_or(token)
            } else {
                token
            };
            first_path = false;
            paths.push(path);
        }
        // Defense in depth: NUL framing already prevents forged records, but
        // a misframed record (a future git output change) would desync —
        // skip anything whose first field isn't a real sha.
        if !is_hex_sha(sha) {
            continue;
        }
        let Ok(committed_at) = ct.parse::<i64>() else {
            continue;
        };
        let commit_node = Node::commit(sha, subject, committed_at);
        let commit_id = commit_node.id.clone();
        graph.nodes.push(commit_node);

        // git already trims surrounding whitespace from idents; trim again
        // so a degenerate email can't key a whitespace Contributor. An
        // empty email can't key one at all (its id would be `contributor:`
        // — no local part, unaddressable): record the commit, but leave it
        // unattributed rather than mint a broken node.
        let email = email.trim();
        if !email.is_empty() {
            let contributor = Node::contributor(email, name);
            let contributor_id = contributor.id.clone();
            if seen_contributors.insert(email.to_string()) {
                graph.nodes.push(contributor);
            }
            graph.edges.push(Edge {
                src: contributor_id,
                dst: commit_id.clone(),
                kind: EdgeKind::Authored,
                provenance: Provenance::Extracted,
                confidence: 1.0,
                location: None,
            });
        }
        for path in paths.into_iter().filter(|p| !p.is_empty()) {
            graph.edges.push(Edge {
                src: commit_id.clone(),
                dst: Node::file(path).id,
                kind: EdgeKind::Touches,
                provenance: Provenance::Extracted,
                confidence: 1.0,
                location: None,
            });
        }
    }
    Ok(graph)
}

/// Whether `s` is a full git object name — 40 (SHA-1) or 64 (SHA-256) hex
/// digits. The `%H` format always emits one; anything else marks a desynced
/// record the parser should drop.
fn is_hex_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Fold an extracted [`extract_history`] graph into the syntactic `graph`,
/// keeping only TOUCHES edges that reach a File node the syntactic pass
/// actually minted. The syntactic graph's File nodes are ground truth for
/// what the current tree holds, so this one rule drops every TOUCHES the
/// read path would choke on — a since-deleted file, a path skipped as
/// non-UTF-8, a submodule gitlink — without the history walk needing to
/// know why. Commit and Contributor nodes and AUTHORED edges always carry
/// over: full history is ingested even for commits that touch nothing in
/// the current tree.
fn merge_history(graph: &mut Graph, history: Graph) {
    let file_ids: std::collections::HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File)
        .map(|n| n.id.as_str())
        .collect();
    let kept_edges: Vec<Edge> = history
        .edges
        .into_iter()
        .filter(|e| e.kind != EdgeKind::Touches || file_ids.contains(e.dst.as_str()))
        .collect();
    drop(file_ids);
    graph.nodes.extend(history.nodes);
    graph.edges.extend(kept_edges);
}

/// Last-resort cap on the history walk. `git log` is local, so the only
/// realistic way it runs long is a sick mirror filesystem — and it runs
/// while the mirror lock is held, so an unbounded run would starve the
/// repo's fetches. Generous (full-history `--name-only` over a large repo
/// still finishes in well under this), but finite.
const HISTORY_LOG_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Walk `git log` over the bare mirror, one NUL-framed record per commit.
/// `-z` NUL-separates commits and (un-quoted) pathnames; `%ct` is the
/// committer date in unix seconds (the `history` Verb's newest-first key);
/// `--no-renames` keeps a rename as delete+add, which is what plain
/// `git log -- <path>` (no `--follow`) reports too, so the demo agrees.
///
/// `--name-only` shows no files for a merge commit (git suppresses merge
/// diffs by default), so a path changed only within a merge gets no
/// TOUCHES — M0 attributes paths per non-merge commit, which agrees with
/// `git log -- <path>` on the linear/first-parent history the tracer
/// targets. Merge-commit path attribution is later-milestone work.
///
/// `kill_on_drop` + a timeout, like `yg_sync`'s `run_git`: a wedged git
/// must not hold the mirror lock forever (the future being dropped reaps
/// the child). Lossy UTF-8 decode: a repo's history is arbitrary, and a
/// latin-1 author name must cost a replacement character at worst, never a
/// pass that fails forever identically. (Paths are kept literal via
/// `core.quotePath=false` so they match the File-node ids the checkout
/// walk built; any that still don't are pruned by `merge_history`.)
async fn git_log_history(git_dir: &Path, commit: &str) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir")
        .arg(git_dir)
        .args(["-c", "core.quotePath=false"])
        .args(["log", commit, "--no-renames", "--name-only", "-z"])
        .arg("--pretty=format:%H%x00%ct%x00%an%x00%ae%x00%s%x00")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let out = tokio::time::timeout(HISTORY_LOG_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "git log {commit} still running after {} minutes; killed \
                 (hung mirror filesystem?)",
                HISTORY_LOG_TIMEOUT.as_secs() / 60
            )
        })?
        .context("running git log (is git installed on this worker?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "git log {commit} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The syntactic pass: walk a materialized checkout and build its graph
/// segment. Every file becomes a File node; Go files additionally yield
/// Symbols and DEFINES edges via tree-sitter (ADR 0002) plus heuristic
/// CALLS / IMPORTS / EXTENDS / IMPLEMENTS edges (ADR 0006).
///
/// Phase 1 parses one file at a time, mints its Symbols, and distills
/// the parse tree into compact `GoFileFacts` — tree and source are
/// released before the next file parses, so memory scales with facts
/// (names and positions), never with a monorepo's worth of parse trees.
/// Phase 2 resolves the facts repo-wide; it cannot run until every file
/// is parsed.
pub fn syntactic_pass(root: &Path) -> anyhow::Result<(Graph, Vec<SearchDoc>)> {
    let mut graph = Graph::default();
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths)?;
    // Walk order must not depend on the filesystem: the graph segment is
    // checksummed, so identical trees should yield identical artifacts.
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .context("loading the Go grammar")?;
    let mut typescript_parser = tree_sitter::Parser::new();
    typescript_parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .context("loading the TypeScript grammar")?;
    let mut tsx_parser = tree_sitter::Parser::new();
    tsx_parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
        .context("loading the TSX grammar")?;
    let mut javascript_parser = tree_sitter::Parser::new();
    javascript_parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .context("loading the JavaScript grammar")?;
    let mut python_parser = tree_sitter::Parser::new();
    python_parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("loading the Python grammar")?;
    let mut rust_parser = tree_sitter::Parser::new();
    rust_parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .context("loading the Rust grammar")?;
    let mut java_parser = tree_sitter::Parser::new();
    java_parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .context("loading the Java grammar")?;
    let mut files = Vec::new();
    let mut simple_files = Vec::new();
    let mut modules = Vec::new();
    // The text of each file, for the full-text segment — valid UTF-8 only
    // (a binary blob is searchable by name alone), keyed by repo-relative
    // path so the Symbol/File documents can be assembled once the graph is
    // built.
    let mut file_text: HashMap<String, String> = HashMap::new();
    for FileEntry { path, is_symlink } in paths {
        let file = Node::file(&path);
        let file_id = file.id.clone();
        graph.nodes.push(file);
        // Symlinks stay content-unread: their target can point anywhere,
        // including outside the checkout.
        if is_symlink {
            continue;
        }
        // Read once: the bytes feed both the Go parse and the full-text
        // body. Other passes read only Go and go.mod, but the segment
        // indexes every file's text (code and markdown alike).
        let bytes = std::fs::read(root.join(&path))
            .with_context(|| format!("reading {path} from the checkout"))?;
        if path.ends_with(".go") {
            if let Some(facts) = extract_go_facts(&mut parser, &path, &file_id, &bytes, &mut graph)
            {
                files.push(facts);
            }
        } else if path.ends_with(".ts") || path.ends_with(".mts") || path.ends_with(".cts") {
            if let Some(facts) = extract_ecmascript_facts(
                &mut typescript_parser,
                &path,
                &file_id,
                &bytes,
                &mut graph,
            ) {
                simple_files.push(facts);
            }
        } else if path.ends_with(".tsx") || path.ends_with(".jsx") {
            if let Some(facts) =
                extract_ecmascript_facts(&mut tsx_parser, &path, &file_id, &bytes, &mut graph)
            {
                simple_files.push(facts);
            }
        } else if path.ends_with(".js") || path.ends_with(".mjs") || path.ends_with(".cjs") {
            if let Some(facts) = extract_ecmascript_facts(
                &mut javascript_parser,
                &path,
                &file_id,
                &bytes,
                &mut graph,
            ) {
                simple_files.push(facts);
            }
        } else if path.ends_with(".py") {
            if let Some(facts) =
                extract_python_facts(&mut python_parser, &path, &file_id, &bytes, &mut graph)
            {
                simple_files.push(facts);
            }
        } else if path.ends_with(".rs") {
            if let Some(facts) =
                extract_rust_facts(&mut rust_parser, &path, &file_id, &bytes, &mut graph)
            {
                simple_files.push(facts);
            }
        } else if path.ends_with(".java") {
            if let Some(facts) =
                extract_java_facts(&mut java_parser, &path, &file_id, &bytes, &mut graph)
            {
                simple_files.push(facts);
            }
        } else if path == "go.mod" || path.ends_with("/go.mod") {
            // Lossy, never fail: synced repos are arbitrary, and a junk
            // file named go.mod must cost module resolution at worst —
            // a failed pass would retry forever, identically.
            if let Some(module) = go_mod_module(&String::from_utf8_lossy(&bytes)) {
                modules.push((package_dir(&path).to_string(), module));
            }
        }
        if let Ok(text) = String::from_utf8(bytes) {
            if text.len() > MAX_BODY_BYTES {
                tracing::debug!(
                    path,
                    bytes = text.len(),
                    cap = MAX_BODY_BYTES,
                    "truncating an oversized file body for the full-text segment"
                );
            }
            file_text.insert(path, cap_body(text));
        }
    }
    let index = SymbolIndex::build(&files, &modules);
    let mut imported = HashSet::new();
    for file in &files {
        emit_import_edges(file, &index, &mut imported, &mut graph);
        emit_call_edges(file, &index, &mut graph);
        emit_extends_edges(file, &index, &mut graph);
    }
    emit_implements_edges(&files, &mut graph);
    let simple_index = SimpleSymbolIndex::build(&simple_files);
    for file in &simple_files {
        emit_simple_import_edges(file, &mut imported, &mut graph);
        emit_simple_call_edges(file, &simple_index, &mut graph);
    }
    let search_docs = build_search_docs(&graph, &file_text);
    Ok((graph, search_docs))
}

/// The full-text documents for a built graph: one per Symbol (searchable
/// by name) and one per File (searchable by its text), assembled from the
/// graph's nodes and the file text gathered during the walk. Package nodes
/// carry no searchable text and are skipped.
fn build_search_docs(graph: &Graph, file_text: &HashMap<String, String>) -> Vec<SearchDoc> {
    graph
        .nodes
        .iter()
        .filter_map(|node| match node.kind {
            NodeKind::Symbol => Some(SearchDoc {
                node_id: node.id.clone(),
                kind: NodeKind::Symbol,
                name: node.name.clone(),
                path: node.path.clone(),
                content: String::new(),
            }),
            NodeKind::File => {
                let path = node.path.as_deref();
                Some(SearchDoc {
                    node_id: node.id.clone(),
                    kind: NodeKind::File,
                    // A File node carries no name; its file name (the last
                    // path segment) is what a query would spell.
                    name: path.map(|p| file_name(p).to_string()),
                    path: node.path.clone(),
                    content: path
                        .and_then(|p| file_text.get(p))
                        .cloned()
                        .unwrap_or_default(),
                })
            }
            // Package, Commit, and Contributor nodes carry no searchable
            // body; they stay out of the full-text segment.
            NodeKind::Package | NodeKind::Commit | NodeKind::Contributor => None,
        })
        .collect()
}

/// Truncate text to [`MAX_BODY_BYTES`] on a char boundary — search reaches
/// the head of an oversized file, never a torn UTF-8 sequence.
fn cap_body(mut text: String) -> String {
    if text.len() > MAX_BODY_BYTES {
        let mut end = MAX_BODY_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The last path segment of a repo-relative path — a File's searchable
/// name (`README.md`, `main.go`).
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The module path a go.mod declares, if any.
fn go_mod_module(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        // Trailing line comments are legal on the directive:
        // `module example.com/m // renamed in v2`.
        let line = line.split("//").next().unwrap_or(line);
        line.trim()
            .strip_prefix("module")
            // "module" must be the whole directive word, not a prefix of
            // an identifier ("modules_test" says nothing about a module).
            .filter(|rest| rest.starts_with([' ', '\t']))
            .map(|rest| rest.trim().trim_matches('"').to_string())
            .filter(|module| !module.is_empty())
    })
}

/// Confidence of a syntactic name resolution with a single candidate —
/// high, but never the 1.0 of a witnessed fact (a DEFINES declaration,
/// an IMPORTS statement): which symbol a name refers to is still a
/// guess. N candidates split it N ways (ADR 0006).
const SYNTACTIC_MATCH: f64 = 0.9;

/// Confidence cap for IMPLEMENTS, which matches method *names* only —
/// signatures are invisible to a reasonable syntactic pass, so even a
/// unique match stays a coin flip (ADR 0006).
const NAME_ONLY_MATCH: f64 = 0.5;

/// What phase 1 distills one Go file into: every name phase 2 must
/// resolve, every site it cites — and nothing else. The parse tree and
/// source are gone by the time this exists.
struct GoFileFacts {
    /// The File node's id, exactly as phase 1 minted it.
    file_id: String,
    /// The file's directory — Go's package boundary for scoping (one
    /// package per directory, with rare exceptions like `_test`
    /// packages that heuristic resolution accepts conflating).
    dir: String,
    /// Imports in declaration order.
    imports: Vec<GoImport>,
    /// Call sites in document order.
    calls: Vec<GoCall>,
    /// Embedded-type references in document order.
    embeds: Vec<GoEmbed>,
    /// Declared functions, `(bare name, symbol id)`, declaration order.
    functions: Vec<(String, String)>,
    /// Declared methods, declaration order.
    methods: Vec<GoMethod>,
    /// Declared types, declaration order.
    types: Vec<GoType>,
    /// Whether the file has a dot import (`import . "…"`), which brings
    /// another package's names into this file's scope unqualified. When
    /// set, an unqualified name with no same-package declaration may be
    /// the dot-imported package's, so repo-wide fallback resolution is
    /// suppressed rather than guessing a far-off same-named repo symbol.
    has_dot_import: bool,
}

/// One import spec: the path it names, where it sits, and the name call
/// sites qualify with — None for blank (`_`) and dot (`.`) imports,
/// which are witnessed imports like any other but introduce no
/// qualifying name.
struct GoImport {
    local_name: Option<String>,
    path: String,
    location: String,
    /// A dot import (`import . "…"`): no qualifying name, but it does
    /// pull the package's exported names into unqualified scope.
    dot: bool,
}

/// One call site, attributed to its enclosing declared symbol.
struct GoCall {
    caller_id: String,
    callee: GoReference,
    location: String,
}

/// One embedded type inside a struct or interface declaration.
struct GoEmbed {
    subject_id: String,
    /// Whether the embedding type is an interface. An interface embeds
    /// only interfaces, so when this is set a target that resolves to a
    /// concrete repo type is a generic constraint (`interface { MyInt }`),
    /// not an embedding, and yields no EXTENDS edge.
    subject_is_interface: bool,
    reference: GoReference,
    location: String,
}

/// A name reference, classified at parse time against its spelling and
/// the file's own imports — everything per-file is settled in phase 1;
/// only the repo-wide resolution is left for phase 2.
enum GoReference {
    /// A bare name: resolved with package scoping.
    Unqualified(String),
    /// `pkg.Name` where pkg names one of the file's imports: resolved
    /// inside that import's package alone.
    Imported { import_path: String, name: String },
    /// `x.Name(…)` whose base names no import: a method reference,
    /// resolved repo-wide by bare name — the receiver's type is
    /// invisible syntactically.
    Method(String),
}

/// One declared method. `receiver` is None when the receiver is
/// unreadable (mid-edit code): such a method still answers method-call
/// resolution by name, but cannot join a type's method set.
struct GoMethod {
    receiver: Option<String>,
    name: String,
    id: String,
}

/// One declared type. `interface` describes an interface's method set;
/// None marks a concrete type.
struct GoType {
    name: String,
    id: String,
    interface: Option<InterfaceShape>,
}

/// Minimal facts for syntactic language packs whose M0 contract is
/// Symbols, DEFINES, package IMPORTS, and name-based CALLS.
struct SimpleFileFacts {
    file_id: String,
    imports: Vec<SimpleImport>,
    calls: Vec<SimpleCall>,
    declarations: Vec<(String, String)>,
}

struct SimpleImport {
    path: String,
    location: String,
}

struct SimpleCall {
    caller_id: String,
    callee: String,
    location: String,
}

struct SimpleExtractionCtx<'a, 'b> {
    source: &'a [u8],
    path: &'a str,
    file_id: &'a str,
    graph: &'b mut Graph,
    id_uses: &'b mut HashMap<String, u32>,
    facts: &'b mut SimpleFileFacts,
}

/// An interface declaration's shape, for IMPLEMENTS matching.
struct InterfaceShape {
    /// Method names declared directly in the interface body.
    direct_methods: BTreeSet<String>,
    /// Whether `direct_methods` is the interface's *whole* method set.
    /// False when the interface embeds another interface (whose methods
    /// we can't resolve syntactically) or carries a type constraint
    /// (`A | B`, `~int` — a generic constraint, not a regular
    /// interface). An incomplete set must not drive IMPLEMENTS: matching
    /// on a subset of the required methods would emit false edges for
    /// types that satisfy only the directly-named methods. M1's precise
    /// pass resolves embedded method sets; until then, honest silence.
    complete: bool,
}

/// Everything the repo declares, by the name a reference would spell.
/// Candidate lists hold `(symbol id, package dir)` in declaration order
/// — file walk order, then document order within a file — which is
/// deterministic because edge output is checksummed.
#[derive(Default)]
struct SymbolIndex {
    functions: HashMap<String, Vec<(String, String)>>,
    /// Methods by bare name: `x.Render()` can't see its receiver's
    /// type, so every `*.Render` is a candidate.
    methods: HashMap<String, Vec<(String, String)>>,
    /// Types by name, for embedded-type references.
    types: HashMap<String, Vec<(String, String)>>,
    /// Symbol ids of types that are interfaces — so an interface
    /// embedding only keeps EXTENDS targets that are themselves
    /// interfaces (a concrete target is a generic constraint, not an
    /// embed).
    interface_ids: HashSet<String>,
    /// Go files (their node ids) per directory: the file half of
    /// in-repo IMPORTS edges.
    files_by_dir: HashMap<String, Vec<String>>,
    /// Repo directories per import path, resolved through go.mod module
    /// paths once per distinct path — phase 2 asks per call site.
    import_dirs: HashMap<String, Vec<String>>,
}

#[derive(Default)]
struct SimpleSymbolIndex {
    symbols: HashMap<String, Vec<String>>,
}

impl SimpleSymbolIndex {
    fn build(files: &[SimpleFileFacts]) -> Self {
        let mut index = Self::default();
        for file in files {
            for (name, id) in &file.declarations {
                index
                    .symbols
                    .entry(name.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        index
    }

    fn resolve(&self, name: &str) -> Vec<&str> {
        self.symbols
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(String::as_str)
            .collect()
    }
}

impl SymbolIndex {
    fn build(files: &[GoFileFacts], modules: &[(String, String)]) -> Self {
        let mut index = Self::default();
        for file in files {
            index
                .files_by_dir
                .entry(file.dir.clone())
                .or_default()
                .push(file.file_id.clone());
            for (name, id) in &file.functions {
                index
                    .functions
                    .entry(name.clone())
                    .or_default()
                    .push((id.clone(), file.dir.clone()));
            }
            for method in &file.methods {
                index
                    .methods
                    .entry(method.name.clone())
                    .or_default()
                    .push((method.id.clone(), file.dir.clone()));
            }
            for declared in &file.types {
                index
                    .types
                    .entry(declared.name.clone())
                    .or_default()
                    .push((declared.id.clone(), file.dir.clone()));
                if declared.interface.is_some() {
                    index.interface_ids.insert(declared.id.clone());
                }
            }
            for import in &file.imports {
                if !index.import_dirs.contains_key(&import.path) {
                    let dirs = resolve_import_dirs(modules, &import.path);
                    index.import_dirs.insert(import.path.clone(), dirs);
                }
            }
        }
        index
    }

    /// The repo directories an import path names; empty for external
    /// imports (stdlib, other modules).
    fn dirs_of(&self, import_path: &str) -> &[String] {
        self.import_dirs
            .get(import_path)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Candidates for a call site's callee: functions for plain and
    /// import-qualified names, methods for the rest. `allow_repo_wide`
    /// is false when the file has a dot import (an unqualified name may
    /// be the dot-imported package's, not a far-off repo function's).
    fn resolve_callee(
        &self,
        callee: &GoReference,
        from_dir: &str,
        allow_repo_wide: bool,
    ) -> Vec<&str> {
        match callee {
            GoReference::Unqualified(name) => {
                Self::scoped(&self.functions, name, from_dir, allow_repo_wide)
            }
            GoReference::Imported { import_path, name } => {
                self.in_import(&self.functions, import_path, name)
            }
            GoReference::Method(name) => candidates(&self.methods, name)
                .iter()
                .map(|(id, _)| id.as_str())
                .collect(),
        }
    }

    /// Candidates for an embedded-type reference. Embeddings are never
    /// classified as method references, so that arm resolves to nothing.
    fn resolve_type(
        &self,
        reference: &GoReference,
        from_dir: &str,
        allow_repo_wide: bool,
    ) -> Vec<&str> {
        match reference {
            GoReference::Unqualified(name) => {
                Self::scoped(&self.types, name, from_dir, allow_repo_wide)
            }
            GoReference::Imported { import_path, name } => {
                self.in_import(&self.types, import_path, name)
            }
            GoReference::Method(_) => Vec::new(),
        }
    }

    /// Go scoping for a bare name: candidates in the referencing file's
    /// own package shadow the rest; a name with no local candidate falls
    /// back to repo-wide matching, unless `allow_repo_wide` is false (a
    /// dot import means the name could be external, so don't reach for a
    /// same-named symbol in an unrelated package).
    fn scoped<'i>(
        by_name: &'i HashMap<String, Vec<(String, String)>>,
        name: &str,
        from_dir: &str,
        allow_repo_wide: bool,
    ) -> Vec<&'i str> {
        let all = candidates(by_name, name);
        let same_package: Vec<&str> = all
            .iter()
            .filter(|(_, dir)| dir == from_dir)
            .map(|(id, _)| id.as_str())
            .collect();
        if !same_package.is_empty() {
            same_package
        } else if allow_repo_wide {
            all.iter().map(|(id, _)| id.as_str()).collect()
        } else {
            Vec::new()
        }
    }

    /// A qualified name inside an imported package: candidates from the
    /// import's resolved directories alone. An import the repo doesn't
    /// contain yields nothing.
    fn in_import<'i>(
        &'i self,
        by_name: &'i HashMap<String, Vec<(String, String)>>,
        import_path: &str,
        name: &str,
    ) -> Vec<&'i str> {
        let dirs = self.dirs_of(import_path);
        candidates(by_name, name)
            .iter()
            .filter(|(_, dir)| dirs.contains(dir))
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

/// All declarations of `name`, however scoped.
fn candidates<'i>(
    by_name: &'i HashMap<String, Vec<(String, String)>>,
    name: &str,
) -> &'i [(String, String)] {
    by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
}

/// The repo directories an import path names, via the repo's go.mod
/// module paths: an import of `<module>/<rest>` lives at
/// `<module dir>/<rest>`. The most specific (longest) module path wins
/// — a nested go.mod owns its subtree, so the parent module never also
/// claims it. An import no module path covers is external: no
/// directories.
fn resolve_import_dirs(modules: &[(String, String)], import_path: &str) -> Vec<String> {
    let matches: Vec<(usize, String)> = modules
        .iter()
        .filter_map(|(dir, module)| {
            let rest = if import_path == module {
                Some("")
            } else {
                import_path
                    .strip_prefix(module.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
            };
            rest.map(|rest| {
                let resolved = match (dir.is_empty(), rest.is_empty()) {
                    (_, true) => dir.clone(),
                    (true, false) => rest.to_string(),
                    (false, false) => format!("{dir}/{rest}"),
                };
                (module.len(), resolved)
            })
        })
        .collect();
    let Some(most_specific) = matches.iter().map(|(len, _)| *len).max() else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = matches
        .into_iter()
        .filter(|(len, _)| *len == most_specific)
        .map(|(_, dir)| dir)
        .collect();
    // Two go.mods declaring the same module path (mid-edit) can tie:
    // ambiguity is kept, duplicates are not.
    dirs.sort_unstable();
    dirs.dedup();
    dirs
}

/// One edge per candidate at split confidence — ADR 0006's ambiguity
/// policy in one place: N candidates share SYNTACTIC_MATCH equally,
/// recorded rather than dropped; no candidates, no edges.
fn push_candidate_edges(
    graph: &mut Graph,
    src: &str,
    candidates: &[&str],
    kind: EdgeKind,
    location: &str,
) {
    if candidates.is_empty() {
        return;
    }
    let confidence = SYNTACTIC_MATCH / candidates.len() as f64;
    for target in candidates {
        graph.edges.push(Edge {
            src: src.to_string(),
            dst: (*target).to_string(),
            kind,
            provenance: Provenance::Syntactic,
            confidence,
            location: Some(location.to_string()),
        });
    }
}

/// The `<path>:<line>:<col>` (1-based) site of a parse-tree node. The
/// column is a byte offset within the line (what tree-sitter reports),
/// not a display column — it primarily disambiguates two sites on one
/// line, which would otherwise be byte-identical rows; a consumer that
/// needs a display column maps bytes→characters against the source.
fn site(path: &str, node: tree_sitter::Node<'_>) -> String {
    let position = node.start_position();
    format!("{path}:{}:{}", position.row + 1, position.column + 1)
}

/// IMPORTS edges for one file (phase 2): each import spec connects the
/// File to its package's node (minted once per import path — node ids
/// are the segment's primary key) at confidence 1.0: the statement is
/// witnessed in the source, not guessed; only the pass is syntactic.
/// An import that go.mod places inside this repo additionally connects
/// the File to the package's Go files (RFC 0001 §5: IMPORTS is File →
/// File/Package) — the directory resolution is the heuristic part — but
/// never to the importing file itself: an external `_test` package
/// lives in the very directory it imports. When an import path resolves
/// to several candidate directories (tied module-path declarations),
/// those directories are alternatives, so confidence spreads across
/// them per ADR 0006; the files within one resolved package are all
/// genuinely imported, so they share that directory's confidence rather
/// than splitting it further. The common single-directory case keeps
/// SYNTACTIC_MATCH.
fn emit_import_edges(
    file: &GoFileFacts,
    index: &SymbolIndex,
    imported: &mut HashSet<String>,
    graph: &mut Graph,
) {
    for import in &file.imports {
        let package = Node::package(&import.path);
        let package_id = package.id.clone();
        if imported.insert(package_id.clone()) {
            graph.nodes.push(package);
        }
        graph.edges.push(Edge {
            src: file.file_id.clone(),
            dst: package_id,
            kind: EdgeKind::Imports,
            provenance: Provenance::Syntactic,
            confidence: 1.0,
            location: Some(import.location.clone()),
        });
        let dirs = index.dirs_of(&import.path);
        let confidence = SYNTACTIC_MATCH / dirs.len().max(1) as f64;
        for dir in dirs {
            for target in index
                .files_by_dir
                .get(dir)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                if *target == file.file_id {
                    continue;
                }
                graph.edges.push(Edge {
                    src: file.file_id.clone(),
                    dst: target.clone(),
                    kind: EdgeKind::Imports,
                    provenance: Provenance::Syntactic,
                    confidence,
                    location: Some(import.location.clone()),
                });
            }
        }
    }
}

/// CALLS edges for one file (phase 2): each collected call site
/// resolves against the repo's declarations — N candidates at
/// SYNTACTIC_MATCH/N each (ADR 0006); a name the repo doesn't declare
/// (stdlib, external packages, builtins) yields nothing.
fn emit_call_edges(file: &GoFileFacts, index: &SymbolIndex, graph: &mut Graph) {
    let allow_repo_wide = !file.has_dot_import;
    for call in &file.calls {
        let candidates = index.resolve_callee(&call.callee, &file.dir, allow_repo_wide);
        push_candidate_edges(
            graph,
            &call.caller_id,
            &candidates,
            EdgeKind::Calls,
            &call.location,
        );
    }
}

/// EXTENDS edges for one file (phase 2): every embedded type in a
/// struct or interface declaration extends the embedding type, resolved
/// like a call target. An interface subject keeps only interface
/// targets — a concrete target named by an interface is a generic
/// constraint (`interface { MyInt }`), not an embedding.
fn emit_extends_edges(file: &GoFileFacts, index: &SymbolIndex, graph: &mut Graph) {
    let allow_repo_wide = !file.has_dot_import;
    for embed in &file.embeds {
        let mut candidates = index.resolve_type(&embed.reference, &file.dir, allow_repo_wide);
        if embed.subject_is_interface {
            candidates.retain(|id| index.interface_ids.contains(*id));
        }
        push_candidate_edges(
            graph,
            &embed.subject_id,
            &candidates,
            EdgeKind::Extends,
            &embed.location,
        );
    }
}

/// IMPLEMENTS edges (phase 2): a type whose method names cover an
/// interface's directly declared method names IMPLEMENTS it (RFC 0001
/// §5), repo-wide — Go interfaces are satisfied across package
/// boundaries. Matching is by name only (signatures are invisible to a
/// reasonable syntactic pass), so confidence is capped at
/// NAME_ONLY_MATCH however unique the match; an interface with no
/// direct methods (`any`, `interface{}`, embeddings only) matches
/// nothing — everything satisfies it, so edges to it would be noise.
/// No location: the relationship has no single site.
///
/// Candidates come from an inverted method-name index, seeded by each
/// interface's rarest method, so cost tracks actual near-matches
/// instead of types × interfaces.
fn emit_implements_edges(files: &[GoFileFacts], graph: &mut Graph) {
    // Method sets per (package dir, receiver type name) — Go only
    // permits methods in the receiver type's own package, so the pair
    // identifies the type. Slot-indexed so everything downstream
    // iterates in first-declaration order: edge output is checksummed,
    // and HashMap order must never leak into it.
    let mut receiver_slots: HashMap<(&str, &str), usize> = HashMap::new();
    let mut receivers: Vec<((&str, &str), BTreeSet<&str>)> = Vec::new();
    for file in files {
        for method in &file.methods {
            let Some(receiver) = &method.receiver else {
                continue;
            };
            let key = (file.dir.as_str(), receiver.as_str());
            let slot = *receiver_slots.entry(key).or_insert_with(|| {
                receivers.push((key, BTreeSet::new()));
                receivers.len() - 1
            });
            receivers[slot].1.insert(&method.name);
        }
    }
    // Inverted: method name → receivers declaring it, declaration order.
    let mut by_method: HashMap<&str, Vec<usize>> = HashMap::new();
    for (slot, (_, names)) in receivers.iter().enumerate() {
        for name in names {
            by_method.entry(name).or_default().push(slot);
        }
    }
    // Concrete types by (dir, name): the IMPLEMENTS sources (every type
    // that is not an interface).
    let mut concrete: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for file in files {
        for declared in &file.types {
            if declared.interface.is_none() {
                concrete
                    .entry((file.dir.as_str(), declared.name.as_str()))
                    .or_default()
                    .push(&declared.id);
            }
        }
    }
    for file in files {
        for declared in &file.types {
            // Only interfaces whose *whole* method set is known here can
            // be matched: an interface that embeds another (or is a
            // generic constraint) would match on a subset and emit false
            // edges (see `InterfaceShape::complete`). Empty interfaces
            // (`any`) match nothing — everything satisfies them.
            let Some(shape) = &declared.interface else {
                continue;
            };
            if !shape.complete || shape.direct_methods.is_empty() {
                continue;
            }
            let needed = &shape.direct_methods;
            // Every needed method must have declarers at all; then the
            // rarest one's posting list seeds the candidate set.
            let postings: Option<Vec<&Vec<usize>>> = needed
                .iter()
                .map(|name| by_method.get(name.as_str()))
                .collect();
            let Some(postings) = postings else {
                continue;
            };
            let seed = postings
                .iter()
                .min_by_key(|posting| posting.len())
                .expect("needed is non-empty");
            for &slot in *seed {
                let (key, methods) = &receivers[slot];
                if !needed.iter().all(|name| methods.contains(name.as_str())) {
                    continue;
                }
                for type_id in concrete.get(key).map(Vec::as_slice).unwrap_or(&[]) {
                    graph.edges.push(Edge {
                        src: type_id.to_string(),
                        dst: declared.id.clone(),
                        kind: EdgeKind::Implements,
                        provenance: Provenance::Syntactic,
                        confidence: NAME_ONLY_MATCH,
                        location: None,
                    });
                }
            }
        }
    }
}

fn emit_simple_import_edges(
    file: &SimpleFileFacts,
    imported: &mut HashSet<String>,
    graph: &mut Graph,
) {
    for import in &file.imports {
        let package = Node::package(&import.path);
        let package_id = package.id.clone();
        if imported.insert(package_id.clone()) {
            graph.nodes.push(package);
        }
        graph.edges.push(Edge {
            src: file.file_id.clone(),
            dst: package_id,
            kind: EdgeKind::Imports,
            provenance: Provenance::Syntactic,
            confidence: 1.0,
            location: Some(import.location.clone()),
        });
    }
}

fn emit_simple_call_edges(file: &SimpleFileFacts, index: &SimpleSymbolIndex, graph: &mut Graph) {
    for call in &file.calls {
        let candidates = index.resolve(&call.callee);
        push_candidate_edges(
            graph,
            &call.caller_id,
            &candidates,
            EdgeKind::Calls,
            &call.location,
        );
    }
}

/// Phase 1 for one Go file: parse, mint its Symbols and DEFINES edges,
/// and distill the tree into [`GoFileFacts`]. Returns None when
/// tree-sitter produces no tree (it only gives up on
/// timeouts/cancellation, neither of which we set) — no symbols rather
/// than a failed pass.
fn extract_go_facts(
    parser: &mut tree_sitter::Parser,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<GoFileFacts> {
    let Some(tree) = parser.parse(source, None) else {
        tracing::warn!(path, "tree-sitter produced no tree; skipping symbols");
        return None;
    };
    let root = tree.root_node();
    let imports = extract_go_imports(root, path, source);
    let mut facts = GoFileFacts {
        file_id: file_id.to_string(),
        dir: package_dir(path).to_string(),
        has_dot_import: imports.iter().any(|import| import.dot),
        imports,
        calls: Vec::new(),
        embeds: Vec::new(),
        functions: Vec::new(),
        methods: Vec::new(),
        types: Vec::new(),
    };
    // Duplicate names (multiple `func init()`, redeclarations mid-edit)
    // must still mint unique node ids — the graph segment keys on them.
    let mut id_uses: HashMap<String, u32> = HashMap::new();
    // Mint one Symbol (node + DEFINES edge), returning its id.
    let mut mint = |name: &str, graph: &mut Graph| {
        let uses = id_uses.entry(name.to_string()).or_insert(0);
        *uses += 1;
        let symbol = Node::symbol(path, name, *uses);
        let symbol_id = symbol.id.clone();
        graph.nodes.push(symbol);
        graph.edges.push(Edge {
            src: file_id.to_string(),
            dst: symbol_id.clone(),
            kind: EdgeKind::Defines,
            provenance: Provenance::Syntactic,
            // The declaration is right there in the parse tree; what
            // is syntactic about it is the pass, not any guesswork.
            confidence: 1.0,
            // A DEFINES edge's site is the declaration itself, which
            // the Symbol node already locates.
            location: None,
        });
        symbol_id
    };
    let mut cursor = root.walk();
    for declaration in root.children(&mut cursor) {
        // CONTEXT.md's Symbol: function, method, type, constant. Each
        // top-level Go declaration of those kinds names one or more.
        match declaration.kind() {
            "function_declaration" => {
                let Some(name) = field_text(declaration, "name", source) else {
                    continue;
                };
                let id = mint(name, graph);
                facts.functions.push((name.to_string(), id.clone()));
                collect_call_sites(
                    declaration,
                    source,
                    &id,
                    &facts.imports,
                    path,
                    &mut facts.calls,
                );
            }
            // Methods are receiver-qualified (Widget.Render): two types'
            // same-named methods are different Symbols.
            "method_declaration" => {
                let Some(name) = field_text(declaration, "name", source) else {
                    continue;
                };
                let receiver = receiver_type_name(declaration, source);
                let qualified = match receiver {
                    Some(receiver) => format!("{receiver}.{name}"),
                    None => name.to_string(),
                };
                let id = mint(&qualified, graph);
                facts.methods.push(GoMethod {
                    receiver: receiver.map(str::to_string),
                    name: name.to_string(),
                    id: id.clone(),
                });
                collect_call_sites(
                    declaration,
                    source,
                    &id,
                    &facts.imports,
                    path,
                    &mut facts.calls,
                );
            }
            // One declaration can hold many specs: type ( A …; B … ),
            // const ( X = 1; Y = 2 ) — and one const spec many names.
            "type_declaration" | "const_declaration" => {
                let mut specs = declaration.walk();
                for spec in declaration.children(&mut specs) {
                    if !matches!(spec.kind(), "type_spec" | "type_alias" | "const_spec") {
                        continue;
                    }
                    let mut names = spec.walk();
                    let names: Vec<String> = spec
                        .children_by_field_name("name", &mut names)
                        .filter_map(|n| n.utf8_text(source).ok().map(str::to_string))
                        .collect();
                    for name in names {
                        let id = mint(&name, graph);
                        if matches!(spec.kind(), "type_spec" | "type_alias") {
                            collect_type_facts(
                                spec,
                                source,
                                &name,
                                &id,
                                &facts.imports,
                                path,
                                &mut facts.types,
                                &mut facts.embeds,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Some(facts)
}

fn extract_ecmascript_facts(
    parser: &mut tree_sitter::Parser,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<SimpleFileFacts> {
    let Some(tree) = parser.parse(source, None) else {
        tracing::warn!(path, "tree-sitter produced no tree; skipping symbols");
        return None;
    };
    let root = tree.root_node();
    let mut facts = SimpleFileFacts {
        file_id: file_id.to_string(),
        imports: extract_ecmascript_imports(root, path, source),
        calls: Vec::new(),
        declarations: Vec::new(),
    };
    let mut id_uses: HashMap<String, u32> = HashMap::new();
    let mut cursor = root.walk();
    for declaration in root.children(&mut cursor) {
        collect_ecmascript_top_level_declaration(
            declaration,
            source,
            path,
            file_id,
            graph,
            &mut id_uses,
            &mut facts,
        );
    }
    Some(facts)
}

fn collect_ecmascript_top_level_declaration(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    file_id: &str,
    graph: &mut Graph,
    id_uses: &mut HashMap<String, u32>,
    facts: &mut SimpleFileFacts,
) {
    if declaration.kind() == "export_statement" {
        let mut cursor = declaration.walk();
        for child in declaration.children(&mut cursor) {
            collect_ecmascript_top_level_declaration(
                child, source, path, file_id, graph, id_uses, facts,
            );
        }
        return;
    }
    match declaration.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "type_alias_declaration"
        | "enum_declaration" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_simple_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id.clone()));
            collect_ecmascript_calls(declaration, source, &id, path, &mut facts.calls);
        }
        "interface_declaration" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_simple_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id));
            let mut ctx = SimpleExtractionCtx {
                source,
                path,
                file_id,
                graph,
                id_uses,
                facts,
            };
            collect_ecmascript_interface_methods(declaration, name, &mut ctx);
        }
        "class_declaration" | "abstract_class_declaration" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_simple_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id));
            let mut ctx = SimpleExtractionCtx {
                source,
                path,
                file_id,
                graph,
                id_uses,
                facts,
            };
            collect_ecmascript_class_methods(declaration, name, &mut ctx);
        }
        "lexical_declaration" | "variable_declaration" => {
            for declarator in descendants_of_kind(declaration, "variable_declarator") {
                let Some(name_node) = declarator
                    .child_by_field_name("name")
                    .filter(|n| n.kind() == "identifier")
                else {
                    continue;
                };
                let Some(name) = name_node.utf8_text(source).ok() else {
                    continue;
                };
                let id = mint_simple_symbol(path, file_id, name, id_uses, graph);
                facts.declarations.push((name.to_string(), id.clone()));
                collect_ecmascript_calls(declarator, source, &id, path, &mut facts.calls);
            }
        }
        _ => {}
    }
}

fn collect_ecmascript_class_methods(
    class_declaration: tree_sitter::Node<'_>,
    class_name: &str,
    ctx: &mut SimpleExtractionCtx<'_, '_>,
) {
    let Some(body) = class_declaration.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() != "method_definition" {
            continue;
        }
        let Some(name) = field_text(item, "name", ctx.source).map(clean_property_name) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let qualified = format!("{class_name}.{name}");
        let id = mint_simple_symbol(ctx.path, ctx.file_id, &qualified, ctx.id_uses, ctx.graph);
        ctx.facts.declarations.push((name.to_string(), id.clone()));
        collect_ecmascript_calls(item, ctx.source, &id, ctx.path, &mut ctx.facts.calls);
    }
}

fn collect_ecmascript_interface_methods(
    interface_declaration: tree_sitter::Node<'_>,
    interface_name: &str,
    ctx: &mut SimpleExtractionCtx<'_, '_>,
) {
    let Some(body) = interface_declaration.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() != "method_signature" {
            continue;
        }
        let Some(name) = field_text(item, "name", ctx.source).map(clean_property_name) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let qualified = format!("{interface_name}.{name}");
        let id = mint_simple_symbol(ctx.path, ctx.file_id, &qualified, ctx.id_uses, ctx.graph);
        ctx.facts.declarations.push((name.to_string(), id));
    }
}

fn clean_property_name(name: &str) -> &str {
    name.trim_matches(['"', '\''])
}

fn mint_simple_symbol(
    path: &str,
    file_id: &str,
    name: &str,
    id_uses: &mut HashMap<String, u32>,
    graph: &mut Graph,
) -> String {
    let uses = id_uses.entry(name.to_string()).or_insert(0);
    *uses += 1;
    let symbol = Node::symbol(path, name, *uses);
    let symbol_id = symbol.id.clone();
    graph.nodes.push(symbol);
    graph.edges.push(Edge {
        src: file_id.to_string(),
        dst: symbol_id.clone(),
        kind: EdgeKind::Defines,
        provenance: Provenance::Syntactic,
        confidence: 1.0,
        location: None,
    });
    symbol_id
}

fn extract_ecmascript_imports(
    root: tree_sitter::Node<'_>,
    path: &str,
    source: &[u8],
) -> Vec<SimpleImport> {
    descendants_of_kind(root, "import_statement")
        .into_iter()
        .filter_map(|statement| {
            let import_path = field_text(statement, "source", source)?
                .trim_matches(['"', '\''])
                .to_string();
            if import_path.is_empty() || import_path.starts_with('.') {
                return None;
            }
            Some(SimpleImport {
                path: import_path,
                location: site(path, statement),
            })
        })
        .collect()
}

fn collect_ecmascript_calls(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    path: &str,
    calls: &mut Vec<SimpleCall>,
) {
    for call in descendants_of_kind(declaration, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let Some(callee) = ecmascript_callee_name(function, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
    for call in descendants_of_kind(declaration, "new_expression") {
        let Some(constructor) = call.child_by_field_name("constructor") else {
            continue;
        };
        let Some(callee) = simple_expression_name(constructor, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
}

fn simple_expression_name<'a>(
    expression: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<&'a str> {
    match expression.kind() {
        "identifier" | "type_identifier" => expression.utf8_text(source).ok(),
        "scoped_identifier" => field_text(expression, "name", source),
        _ => None,
    }
}

fn ecmascript_callee_name<'a>(
    expression: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<&'a str> {
    match expression.kind() {
        "identifier" => expression.utf8_text(source).ok(),
        "member_expression" => {
            let object = expression
                .child_by_field_name("object")
                .and_then(|node| node.utf8_text(source).ok());
            if object == Some("this") {
                field_text(expression, "property", source)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn extract_rust_facts(
    parser: &mut tree_sitter::Parser,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<SimpleFileFacts> {
    let Some(tree) = parser.parse(source, None) else {
        tracing::warn!(path, "tree-sitter produced no tree; skipping symbols");
        return None;
    };
    let root = tree.root_node();
    let mut facts = SimpleFileFacts {
        file_id: file_id.to_string(),
        imports: extract_rust_imports(root, path, source),
        calls: Vec::new(),
        declarations: Vec::new(),
    };
    let mut id_uses: HashMap<String, u32> = HashMap::new();
    let mut cursor = root.walk();
    for declaration in root.children(&mut cursor) {
        match declaration.kind() {
            "function_item" | "struct_item" | "enum_item" | "trait_item" | "const_item"
            | "static_item" | "type_item" => {
                let Some(name) = field_text(declaration, "name", source) else {
                    continue;
                };
                let id = mint_simple_symbol(path, file_id, name, &mut id_uses, graph);
                facts.declarations.push((name.to_string(), id.clone()));
                collect_rust_calls(declaration, source, &id, path, &mut facts.calls);
            }
            "impl_item" => collect_rust_impl_item(
                declaration,
                source,
                path,
                file_id,
                graph,
                &mut id_uses,
                &mut facts,
            ),
            _ => {}
        }
    }
    Some(facts)
}

fn collect_rust_impl_item(
    impl_item: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    file_id: &str,
    graph: &mut Graph,
    id_uses: &mut HashMap<String, u32>,
    facts: &mut SimpleFileFacts,
) {
    let Some(receiver) = impl_item
        .child_by_field_name("type")
        .and_then(|node| rust_type_name(node, source))
    else {
        return;
    };
    let Some(body) = impl_item.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() != "function_item" {
            continue;
        }
        let Some(name) = field_text(item, "name", source) else {
            continue;
        };
        let qualified = format!("{receiver}.{name}");
        let id = mint_simple_symbol(path, file_id, &qualified, id_uses, graph);
        facts.declarations.push((name.to_string(), id.clone()));
        collect_rust_calls(item, source, &id, path, &mut facts.calls);
    }
}

fn rust_type_name<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "type_identifier" | "primitive_type" => node.utf8_text(source).ok(),
        "generic_type" => field_text(node, "type", source),
        _ => first_of_kind(node, "type_identifier")
            .or_else(|| first_of_kind(node, "primitive_type"))
            .and_then(|n| n.utf8_text(source).ok()),
    }
}

fn extract_rust_imports(
    root: tree_sitter::Node<'_>,
    path: &str,
    source: &[u8],
) -> Vec<SimpleImport> {
    descendants_of_kind(root, "use_declaration")
        .into_iter()
        .filter_map(|declaration| {
            let argument = declaration.child_by_field_name("argument")?;
            if argument
                .utf8_text(source)
                .ok()
                .is_some_and(is_rust_internal_use_path)
            {
                return None;
            }
            let package = rust_use_root(argument, source)?;
            Some(SimpleImport {
                path: package.to_string(),
                location: site(path, declaration),
            })
        })
        .collect()
}

fn is_rust_internal_use_path(path: &str) -> bool {
    matches!(path, "crate" | "self" | "super")
        || path.starts_with("crate::")
        || path.starts_with("self::")
        || path.starts_with("super::")
}

fn rust_use_root<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "identifier" => node.utf8_text(source).ok(),
        "scoped_identifier" => node
            .child_by_field_name("path")
            .and_then(|path| rust_use_root(path, source)),
        "use_as_clause" | "scoped_use_list" => first_of_kind(node, "identifier")
            .and_then(|identifier| identifier.utf8_text(source).ok()),
        _ => None,
    }
}

fn collect_rust_calls(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    path: &str,
    calls: &mut Vec<SimpleCall>,
) {
    for call in descendants_of_kind(declaration, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let Some(callee) = rust_callee_name(function, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
}

fn rust_callee_name<'a>(expression: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    simple_expression_name(expression, source).or_else(|| last_identifier_name(expression, source))
}

fn last_identifier_name<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    descendants_of_kind(node, "identifier")
        .into_iter()
        .last()
        .and_then(|identifier| identifier.utf8_text(source).ok())
}

fn extract_java_facts(
    parser: &mut tree_sitter::Parser,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<SimpleFileFacts> {
    let Some(tree) = parser.parse(source, None) else {
        tracing::warn!(path, "tree-sitter produced no tree; skipping symbols");
        return None;
    };
    let root = tree.root_node();
    let mut facts = SimpleFileFacts {
        file_id: file_id.to_string(),
        imports: extract_java_imports(root, path, source),
        calls: Vec::new(),
        declarations: Vec::new(),
    };
    let mut id_uses: HashMap<String, u32> = HashMap::new();
    let mut ctx = SimpleExtractionCtx {
        source,
        path,
        file_id,
        graph,
        id_uses: &mut id_uses,
        facts: &mut facts,
    };
    collect_java_declarations(root, &mut ctx);
    Some(facts)
}

fn collect_java_declarations(root: tree_sitter::Node<'_>, ctx: &mut SimpleExtractionCtx<'_, '_>) {
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        let declaration = cursor.node();
        if is_java_declaration_kind(declaration.kind()) {
            collect_java_declaration(declaration, ctx);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() || cursor.node() == root {
                return;
            }
        }
    }
}

fn is_java_declaration_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "enum_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "field_declaration"
            | "method_declaration"
    )
}

fn collect_java_declaration(
    declaration: tree_sitter::Node<'_>,
    ctx: &mut SimpleExtractionCtx<'_, '_>,
) {
    match declaration.kind() {
        "class_declaration"
        | "enum_declaration"
        | "interface_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            let Some(name) = field_text(declaration, "name", ctx.source) else {
                return;
            };
            let id = mint_simple_symbol(ctx.path, ctx.file_id, name, ctx.id_uses, ctx.graph);
            ctx.facts.declarations.push((name.to_string(), id));
        }
        "method_declaration" => {
            let Some(name) = field_text(declaration, "name", ctx.source) else {
                return;
            };
            let symbol_name = java_member_symbol_name(declaration, ctx.source, name);
            let id =
                mint_simple_symbol(ctx.path, ctx.file_id, &symbol_name, ctx.id_uses, ctx.graph);
            ctx.facts.declarations.push((name.to_string(), id.clone()));
            collect_java_calls(declaration, ctx.source, &id, ctx.path, &mut ctx.facts.calls);
        }
        "field_declaration" => {
            let mut cursor = declaration.walk();
            for declarator in declaration.children_by_field_name("declarator", &mut cursor) {
                let Some(name) = field_text(declarator, "name", ctx.source) else {
                    continue;
                };
                let symbol_name = java_member_symbol_name(declaration, ctx.source, name);
                let id =
                    mint_simple_symbol(ctx.path, ctx.file_id, &symbol_name, ctx.id_uses, ctx.graph);
                ctx.facts.declarations.push((name.to_string(), id.clone()));
                collect_java_calls(declarator, ctx.source, &id, ctx.path, &mut ctx.facts.calls);
            }
        }
        _ => {}
    }
}

fn java_member_symbol_name(member: tree_sitter::Node<'_>, source: &[u8], name: &str) -> String {
    match java_containing_type_name(member, source) {
        Some(container) => format!("{container}.{name}"),
        None => name.to_string(),
    }
}

fn java_containing_type_name<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut current = node.parent();
    while let Some(node) = current {
        if matches!(
            node.kind(),
            "class_declaration"
                | "enum_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
        ) {
            return field_text(node, "name", source);
        }
        current = node.parent();
    }
    None
}

fn extract_java_imports(
    root: tree_sitter::Node<'_>,
    path: &str,
    source: &[u8],
) -> Vec<SimpleImport> {
    descendants_of_kind(root, "import_declaration")
        .into_iter()
        .filter_map(|declaration| {
            let mut cursor = declaration.walk();
            let imported = declaration
                .named_children(&mut cursor)
                .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier"))?
                .utf8_text(source)
                .ok()?;
            Some(SimpleImport {
                path: imported.to_string(),
                location: site(path, declaration),
            })
        })
        .collect()
}

fn collect_java_calls(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    path: &str,
    calls: &mut Vec<SimpleCall>,
) {
    for call in descendants_of_kind(declaration, "method_invocation") {
        if let Some(object) = call.child_by_field_name("object") {
            let object = object.utf8_text(source).ok();
            if !matches!(object, Some("this" | "super")) {
                continue;
            }
        }
        let Some(callee) = field_text(call, "name", source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
    for call in descendants_of_kind(declaration, "object_creation_expression") {
        let Some(created_type) = call.child_by_field_name("type") else {
            continue;
        };
        let Some(callee) = simple_expression_name(created_type, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
}

fn extract_python_facts(
    parser: &mut tree_sitter::Parser,
    path: &str,
    file_id: &str,
    source: &[u8],
    graph: &mut Graph,
) -> Option<SimpleFileFacts> {
    let Some(tree) = parser.parse(source, None) else {
        tracing::warn!(path, "tree-sitter produced no tree; skipping symbols");
        return None;
    };
    let root = tree.root_node();
    let mut facts = SimpleFileFacts {
        file_id: file_id.to_string(),
        imports: extract_python_imports(root, path, source),
        calls: Vec::new(),
        declarations: Vec::new(),
    };
    let mut id_uses: HashMap<String, u32> = HashMap::new();
    let mut cursor = root.walk();
    for declaration in root.children(&mut cursor) {
        collect_python_top_level_declaration(
            declaration,
            source,
            path,
            file_id,
            graph,
            &mut id_uses,
            &mut facts,
        );
    }
    Some(facts)
}

fn collect_python_top_level_declaration(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    file_id: &str,
    graph: &mut Graph,
    id_uses: &mut HashMap<String, u32>,
    facts: &mut SimpleFileFacts,
) {
    if declaration.kind() == "expression_statement" {
        let mut cursor = declaration.walk();
        for child in declaration.children(&mut cursor) {
            collect_python_top_level_declaration(
                child, source, path, file_id, graph, id_uses, facts,
            );
        }
        return;
    }
    match declaration.kind() {
        "class_definition" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_simple_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id));
            let mut ctx = SimpleExtractionCtx {
                source,
                path,
                file_id,
                graph,
                id_uses,
                facts,
            };
            collect_python_class_methods(declaration, name, &mut ctx);
        }
        "function_definition" => {
            let Some(name) = field_text(declaration, "name", source) else {
                return;
            };
            let id = mint_simple_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id.clone()));
            collect_python_calls(declaration, source, &id, path, &mut facts.calls);
        }
        "assignment" => {
            let Some(left) = declaration
                .child_by_field_name("left")
                .filter(|n| n.kind() == "identifier")
            else {
                return;
            };
            let Some(name) = left.utf8_text(source).ok() else {
                return;
            };
            let id = mint_simple_symbol(path, file_id, name, id_uses, graph);
            facts.declarations.push((name.to_string(), id.clone()));
            collect_python_calls(declaration, source, &id, path, &mut facts.calls);
        }
        _ => {}
    }
}

fn collect_python_class_methods(
    class_definition: tree_sitter::Node<'_>,
    class_name: &str,
    ctx: &mut SimpleExtractionCtx<'_, '_>,
) {
    let Some(body) = class_definition.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for item in body.children(&mut cursor) {
        if item.kind() != "function_definition" {
            continue;
        }
        let Some(name) = field_text(item, "name", ctx.source) else {
            continue;
        };
        let qualified = format!("{class_name}.{name}");
        let id = mint_simple_symbol(ctx.path, ctx.file_id, &qualified, ctx.id_uses, ctx.graph);
        ctx.facts.declarations.push((name.to_string(), id.clone()));
        collect_python_calls(item, ctx.source, &id, ctx.path, &mut ctx.facts.calls);
    }
}

fn extract_python_imports(
    root: tree_sitter::Node<'_>,
    path: &str,
    source: &[u8],
) -> Vec<SimpleImport> {
    let mut imports = Vec::new();
    for statement in descendants_of_kind(root, "import_from_statement") {
        if let Some(module) =
            field_text(statement, "module_name", source).filter(|module| !module.starts_with('.'))
        {
            imports.push(SimpleImport {
                path: module.to_string(),
                location: site(path, statement),
            });
        }
    }
    for statement in descendants_of_kind(root, "import_statement") {
        let mut cursor = statement.walk();
        for name in statement.children_by_field_name("name", &mut cursor) {
            let package = match name.kind() {
                "dotted_name" | "identifier" => name.utf8_text(source).ok(),
                "aliased_import" => first_of_kind(name, "dotted_name")
                    .or_else(|| first_of_kind(name, "identifier"))
                    .and_then(|n| n.utf8_text(source).ok()),
                _ => None,
            };
            let Some(package) = package.filter(|package| !package.is_empty()) else {
                continue;
            };
            imports.push(SimpleImport {
                path: package.to_string(),
                location: site(path, statement),
            });
        }
    }
    imports
}

fn collect_python_calls(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    path: &str,
    calls: &mut Vec<SimpleCall>,
) {
    for call in descendants_of_kind(declaration, "call") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let Some(callee) = python_callee_name(function, source) else {
            continue;
        };
        calls.push(SimpleCall {
            caller_id: caller_id.to_string(),
            callee: callee.to_string(),
            location: site(path, call),
        });
    }
}

fn python_callee_name<'a>(expression: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    match expression.kind() {
        "identifier" => expression.utf8_text(source).ok(),
        "attribute" => {
            let object = expression
                .child_by_field_name("object")
                .and_then(|node| node.utf8_text(source).ok());
            if matches!(object, Some("self" | "cls")) {
                field_text(expression, "attribute", source)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Every call site inside one declaration's subtree, attributed to that
/// declaration's symbol and classified against the file's imports.
fn collect_call_sites(
    declaration: tree_sitter::Node<'_>,
    source: &[u8],
    caller_id: &str,
    imports: &[GoImport],
    path: &str,
    calls: &mut Vec<GoCall>,
) {
    for call in descendants_of_kind(declaration, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let callee = match function.kind() {
            // An unqualified call: a function name.
            "identifier" => match function.utf8_text(source).ok() {
                Some(name) => GoReference::Unqualified(name.to_string()),
                None => continue,
            },
            "selector_expression" => {
                let Some(name) = field_text(function, "field", source) else {
                    continue;
                };
                let base = function
                    .child_by_field_name("operand")
                    .filter(|operand| operand.kind() == "identifier")
                    .and_then(|operand| operand.utf8_text(source).ok());
                match base.and_then(|base| import_named(imports, base)) {
                    // `util.Reset()` where util names an import: a
                    // package-qualified call — never a method.
                    Some(import) => GoReference::Imported {
                        import_path: import.path.clone(),
                        name: name.to_string(),
                    },
                    // `x.Render()`: a method call.
                    None => GoReference::Method(name.to_string()),
                }
            }
            _ => continue,
        };
        calls.push(GoCall {
            caller_id: caller_id.to_string(),
            callee,
            location: site(path, call),
        });
    }
}

/// Facts of one type spec: the declared type itself (with an
/// interface's direct method names) plus its embedded-type references.
#[allow(clippy::too_many_arguments)]
fn collect_type_facts(
    spec: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
    id: &str,
    imports: &[GoImport],
    path: &str,
    types: &mut Vec<GoType>,
    embeds: &mut Vec<GoEmbed>,
) {
    let type_node = spec.child_by_field_name("type");
    let interface = type_node
        .filter(|node| node.kind() == "interface_type")
        .map(|node| interface_shape(node, source));
    let subject_is_interface = interface.is_some();
    types.push(GoType {
        name: name.to_string(),
        id: id.to_string(),
        interface,
    });
    let embedded: Vec<tree_sitter::Node> = match type_node.map(|node| (node.kind(), node)) {
        // An embedded struct field is a field_declaration with no name
        // — only its type, possibly behind a pointer. Direct fields
        // only: a nested anonymous struct's fields embed into that
        // struct, not this type.
        Some(("struct_type", node)) => {
            let mut cursor = node.walk();
            let fields = node
                .children(&mut cursor)
                .find(|n| n.kind() == "field_declaration_list");
            match fields {
                Some(fields) => {
                    let mut cursor = fields.walk();
                    fields
                        .children(&mut cursor)
                        .filter(|field| field.kind() == "field_declaration")
                        .filter(|field| field.child_by_field_name("name").is_none())
                        .collect()
                }
                None => Vec::new(),
            }
        }
        // An embedded interface is a type_elem naming exactly one type
        // (`io.Reader`, `Base`). A type_elem that is a constraint union
        // (`A | B`) or approximation (`~int`) is generics, not
        // embedding — `embedded_interface_type` returns None for those,
        // so they yield no EXTENDS edge (and `A | B` never collapses to
        // a spurious edge to just `A`).
        Some(("interface_type", node)) => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .filter(|elem| elem.kind() == "type_elem")
                .filter_map(embedded_interface_type)
                .collect()
        }
        _ => Vec::new(),
    };
    for embed in embedded {
        let Some((package, type_name)) = embedded_type_reference(embed, source) else {
            continue;
        };
        let reference = match package {
            // `util.Base`: the package half resolves through this
            // file's imports, like a qualified call; a qualifier that
            // names no import resolves to nothing.
            Some(package) => match import_named(imports, package) {
                Some(import) => GoReference::Imported {
                    import_path: import.path.clone(),
                    name: type_name.to_string(),
                },
                None => continue,
            },
            None => GoReference::Unqualified(type_name.to_string()),
        };
        embeds.push(GoEmbed {
            subject_id: id.to_string(),
            subject_is_interface,
            reference,
            location: site(path, embed),
        });
    }
}

/// An interface declaration's method set and whether it is complete.
/// Any `type_elem` (an embedded interface, or a `A | B` / `~int`
/// constraint) makes the set incomplete: the embedded methods aren't
/// resolved here, and a constraint isn't a regular interface at all.
fn interface_shape(interface: tree_sitter::Node<'_>, source: &[u8]) -> InterfaceShape {
    let mut direct_methods = BTreeSet::new();
    let mut complete = true;
    let mut cursor = interface.walk();
    for elem in interface.children(&mut cursor) {
        match elem.kind() {
            "method_elem" => {
                if let Some(name) = field_text(elem, "name", source) {
                    direct_methods.insert(name.to_string());
                }
            }
            "type_elem" => complete = false,
            _ => {}
        }
    }
    InterfaceShape {
        direct_methods,
        complete,
    }
}

/// The single type an interface `type_elem` embeds — `Some` only when
/// the element is a lone type name (`io.Reader`, `Base`). A union
/// (`A | B`, several named children) or an approximation (`~int`, a
/// `negated_type` child) is a generic constraint, not an embedding, and
/// yields `None`.
fn embedded_interface_type(type_elem: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = type_elem.walk();
    let named: Vec<tree_sitter::Node> = type_elem.named_children(&mut cursor).collect();
    match named.as_slice() {
        [only] if matches!(only.kind(), "type_identifier" | "qualified_type") => Some(*only),
        _ => None,
    }
}

/// The type name an embedded field or interface element refers to:
/// `(package, name)` for `util.Base`, `(None, name)` for `Base` or
/// `*Base` — the first qualified or bare type identifier in the
/// embedding, however it is wrapped (pointer, generic instantiation).
fn embedded_type_reference<'a>(
    embed: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<(Option<&'a str>, &'a str)> {
    if let Some(qualified) = first_of_kind(embed, "qualified_type") {
        let package = field_text(qualified, "package", source)?;
        let name = field_text(qualified, "name", source)?;
        return Some((Some(package), name));
    }
    first_of_kind(embed, "type_identifier")
        .and_then(|n| n.utf8_text(source).ok())
        .map(|name| (None, name))
}

/// A Go file's imports: every spec under every import declaration,
/// whether single (`import "fmt"`) or grouped (`import ( … )`). A spec
/// with an empty path (`import ""` — illegal Go, mid-edit garbage) is
/// skipped whole: a `pkg:` node with no path could never round-trip
/// through the external id grammar.
fn extract_go_imports(root: tree_sitter::Node<'_>, path: &str, source: &[u8]) -> Vec<GoImport> {
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    for declaration in root.children(&mut cursor) {
        if declaration.kind() != "import_declaration" {
            continue;
        }
        for spec in descendants_of_kind(declaration, "import_spec") {
            // The path literal keeps its quotes in the parse tree.
            let Some(import_path) = field_text(spec, "path", source)
                .map(|quoted| quoted.trim_matches(['"', '`']).to_string())
                .filter(|p| !p.is_empty())
            else {
                continue;
            };
            let alias = field_text(spec, "name", source);
            let local_name = match alias {
                // Blank and dot imports introduce no qualifying name —
                // but stay in the list: they are witnessed imports.
                Some("_") | Some(".") => None,
                Some(alias) => Some(alias.to_string()),
                // Unaliased: qualified by the path's last segment. (The
                // package's declared name can differ from its directory;
                // a heuristic pass accepts conflating them.)
                None => import_path.rsplit('/').next().map(str::to_string),
            };
            imports.push(GoImport {
                local_name: local_name.filter(|name| !name.is_empty()),
                path: import_path,
                location: site(path, spec),
                dot: alias == Some("."),
            });
        }
    }
    imports
}

/// The file's import a local name refers to — the one lookup both
/// reference classification and resolution share.
fn import_named<'i>(imports: &'i [GoImport], name: &str) -> Option<&'i GoImport> {
    imports
        .iter()
        .find(|import| import.local_name.as_deref() == Some(name))
}

/// The directory holding a repo-relative file path — Go's package
/// boundary for scoping purposes (one package per directory, with rare
/// exceptions like `_test` packages that heuristic resolution accepts
/// conflating).
fn package_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// Every descendant of `node` (excluding itself) with the given kind,
/// in document order — one cursor walk, no per-node allocation.
fn descendants_of_kind<'t>(node: tree_sitter::Node<'t>, kind: &str) -> Vec<tree_sitter::Node<'t>> {
    let mut found = Vec::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return found;
    }
    loop {
        if cursor.node().kind() == kind {
            found.push(cursor.node());
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() || cursor.node() == node {
                return found;
            }
        }
    }
}

/// `node` itself, or its first descendant of the given kind.
fn first_of_kind<'t>(node: tree_sitter::Node<'t>, kind: &str) -> Option<tree_sitter::Node<'t>> {
    if node.kind() == kind {
        return Some(node);
    }
    descendants_of_kind(node, kind).into_iter().next()
}

/// Text of a named field on a node, when present and valid UTF-8.
fn field_text<'a>(node: tree_sitter::Node<'_>, field: &str, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(source).ok())
}

/// The bare type name a method's receiver refers to: the first type
/// identifier inside `(w *Widget)`, however the receiver is spelled
/// (pointer, generic, parenthesized). The search never leaves the
/// receiver subtree, so a receiver without one (mid-edit code) yields
/// None rather than a type stolen from elsewhere in the file.
fn receiver_type_name<'a>(method: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    fn first_type_identifier<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> Option<&'a str> {
        if node.kind() == "type_identifier" {
            return node.utf8_text(source).ok();
        }
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find_map(|child| first_type_identifier(child, source))
    }
    first_type_identifier(method.child_by_field_name("receiver")?, source)
}

/// One tree entry as the walk found it.
struct FileEntry {
    /// Repo-relative, slash-separated path.
    path: String,
    is_symlink: bool,
}

/// Recursively collect every non-directory tree entry. Symlinks count —
/// they are blobs in the git tree — but are flagged so nothing reads
/// through them.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<FileEntry>) -> anyhow::Result<()> {
    // Materialize the listing before descending: recursing with the
    // ReadDir handle open holds one directory fd per nesting level, and
    // a deep-enough committed path chain would run the whole process —
    // API listener included — out of file descriptors.
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("walking {}", dir.display()))? {
        let entry = entry?;
        entries.push((entry.path(), entry.file_type()?));
    }
    for (full, file_type) in entries {
        if file_type.is_dir() {
            collect_files(root, &full, out)?;
        } else {
            let relative = full.strip_prefix(root).expect("walk stays under root");
            // A non-UTF-8 name can't round-trip through the graph's
            // string ids — converting it lossily would point the id at a
            // path that doesn't exist (and could collide with a sibling
            // differing only in the invalid bytes). Skip such entries;
            // skipping is deterministic, so identical trees still yield
            // identical artifacts.
            let Some(components) = relative
                .components()
                .map(|c| c.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
            else {
                tracing::warn!(
                    path = %relative.display(),
                    "skipping a checkout path that is not valid UTF-8"
                );
                continue;
            };
            out.push(FileEntry {
                path: components.join("/"),
                is_symlink: file_type.is_symlink(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Folding history into the syntactic graph keeps a TOUCHES only when
    /// the syntactic pass minted a File node for its target — a since-
    /// deleted (or otherwise skipped) path is pruned so the read path never
    /// meets an edge to an absent node. Commit/Contributor nodes and
    /// AUTHORED edges always carry over (full history is ingested).
    #[test]
    fn merge_history_prunes_touches_to_files_the_syntactic_graph_lacks() {
        let mut graph = Graph::default();
        graph.nodes.push(Node::file("foo.go"));

        let mut history = Graph::default();
        history.nodes.push(Node::commit("abc123", "edit", 1_000));
        history
            .nodes
            .push(Node::contributor("alice@example.com", "Alice"));
        history.edges.push(Edge {
            src: "contributor:alice@example.com".into(),
            dst: "commit:abc123".into(),
            kind: EdgeKind::Authored,
            provenance: Provenance::Extracted,
            confidence: 1.0,
            location: None,
        });
        // The commit touched foo.go (a current File) and scratch.txt (none).
        for path in ["foo.go", "scratch.txt"] {
            history.edges.push(Edge {
                src: "commit:abc123".into(),
                dst: format!("file:{path}"),
                kind: EdgeKind::Touches,
                provenance: Provenance::Extracted,
                confidence: 1.0,
                location: None,
            });
        }

        merge_history(&mut graph, history);

        assert!(graph.nodes.iter().any(|n| n.id == "commit:abc123"));
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| n.id == "contributor:alice@example.com")
        );
        assert_eq!(
            graph
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Authored)
                .count(),
            1,
            "AUTHORED always carries over"
        );
        let touched: Vec<&str> = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Touches)
            .map(|e| e.dst.as_str())
            .collect();
        assert_eq!(
            touched,
            ["file:foo.go"],
            "the absent file's TOUCHES is pruned"
        );
    }
}
