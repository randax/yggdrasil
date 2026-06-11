//! Forge trait + GitHub/GitLab/Forgejo (Codeberg runs Forgejo) adapters, webhooks.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use yg_control::ControlPlane;

/// How long a worker may hold a fetch job before a crashed run becomes
/// claimable again. Generous: a cold full-history clone of a large repo.
const FETCH_LEASE: Duration = Duration::from_secs(15 * 60);

/// A Sync worker: drains the fetch queue, mirroring repos into the
/// worker-local git cache and recording each repo's synced commit.
pub struct SyncWorker {
    control: ControlPlane,
    fetcher: GitFetcher,
}

impl SyncWorker {
    pub fn new(control: ControlPlane, git_cache: impl Into<PathBuf>) -> Self {
        Self {
            control,
            fetcher: GitFetcher::new(git_cache),
        }
    }

    /// Claim and run one due job. Returns whether there was work. A
    /// failed fetch is recorded (with backoff) rather than returned as an
    /// error — `Err` means the control plane itself is unreachable.
    pub async fn run_once(&self) -> anyhow::Result<bool> {
        let Some(job) = self.control.claim_due_fetch(FETCH_LEASE).await? else {
            return Ok(false);
        };
        let clone_url = join_clone_url(&job.base_url, &job.slug);
        let token = forge_token(job.token_env.as_deref(), &clone_url);
        // A lock failure (cache dir on a dead disk) is a failed fetch,
        // not a dead worker: record it and let backoff retry.
        let synced = async {
            let _serialize_same_mirror =
                lock_mirror(self.fetcher.cache_dir(), job.repo_id, FETCH_LEASE).await?;
            self.fetcher
                .sync(job.repo_id, &clone_url, token.as_deref(), job.fetch_depth)
                .await
        }
        .await;
        match synced {
            Ok(commit) => {
                if self.control.complete_fetch(&job, &commit).await? {
                    tracing::info!(slug = %job.slug, %commit, "synced");
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-fetch; result discarded");
                }
            }
            Err(e) => {
                let error = format!("{e:#}");
                if self.control.fail_fetch(&job, &error).await? {
                    tracing::warn!(slug = %job.slug, attempt = job.attempts + 1, error, "fetch failed");
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-fetch; failure discarded");
                }
            }
        }
        Ok(true)
    }
}

/// Resolve the Forge token for a clone: read the env var the control
/// plane names, if any. Defense in depth: whatever the control plane
/// says, a Forge token only ever travels over TLS.
pub fn forge_token(token_env: Option<&str>, clone_url: &str) -> Option<String> {
    token_env
        .and_then(|var| std::env::var(var).ok())
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .filter(|_| clone_url.starts_with("https://"))
}

/// Where repo `repo_id`'s bare mirror lives inside a worker's git cache.
/// The one definition of the cache layout — the fetch side writes here
/// and the indexing side reads here.
pub fn mirror_path(cache_dir: &std::path::Path, repo_id: i64) -> PathBuf {
    cache_dir.join(format!("{repo_id}.git"))
}

/// Mirrors remote repositories into a local cache of bare clones, one
/// per repo at [`mirror_path`]. Used by the Sync worker on every fetch
/// job, and by indexing workers to populate their local cache when a
/// job lands on a host that hasn't fetched the repo.
pub struct GitFetcher {
    cache_dir: PathBuf,
}

impl GitFetcher {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    /// The cache dir this fetcher mirrors into — what [`lock_mirror`]
    /// guards and [`mirror_path`] resolves against.
    pub fn cache_dir(&self) -> &std::path::Path {
        &self.cache_dir
    }

    /// Bare-clone `clone_url` on first sight, fetch it afterwards; either
    /// way the cache ends at the remote's current state. Returns the
    /// commit the remote's default branch points at.
    ///
    /// A mirror that exists but isn't usable (interrupted clone, stray
    /// deletion) is discarded and re-cloned rather than left to fail
    /// every retry forever.
    ///
    /// Callers hold the repo's [`lock_mirror`] guard across this call
    /// (and any reads of the mirror they do around it) — the lock is not
    /// taken here so a caller can keep it across a fetch-then-read
    /// sequence.
    pub async fn sync(
        &self,
        repo_id: i64,
        clone_url: &str,
        token: Option<&str>,
        depth: Option<i32>,
    ) -> anyhow::Result<String> {
        let local = mirror_path(&self.cache_dir, repo_id);
        let depth_arg = depth.map(|n| format!("--depth={n}"));
        sweep_stale_partials(&self.cache_dir, repo_id).await;
        // A bare repo's skeleton: a well-formed HEAD plus objects/ and
        // refs/. Anything less is wreckage to re-clone — a crash can
        // leave HEAD zero-byte or NUL-filled, a torn restore can drop
        // objects/ — and git would fail "not a git repository" on
        // every retry forever. Deliberately file reads, not a git
        // probe: a probe conflates "git said no" with "git could not
        // run" (missing binary, fd pressure) and would delete a
        // healthy mirror over an environmental blip — and a HEAD that
        // exists but dangles is healed after the fetch by the re-point
        // below, not by re-downloading history that would dangle again.
        let usable = head_names_a_ref(&local.join("HEAD"))
            && local.join("objects").is_dir()
            && local.join("refs").is_dir();
        if usable {
            let mut args: Vec<&str> = vec!["fetch", "--prune", "--quiet"];
            args.extend(depth_arg.as_deref());
            // git never deepens a shallow mirror on its own: when the
            // depth override is gone but the mirror is still shallow,
            // ask for the rest of history explicitly.
            if depth.is_none() && local.join("shallow").exists() {
                args.push("--unshallow");
            }
            // clone --bare configures no fetch refspec; mirror branches
            // explicitly (refs/heads only — not refs/*, which on GitHub
            // would drag in every change request's head).
            args.extend(["origin", "+refs/heads/*:refs/heads/*"]);
            run_git(Some(&local), &args, token)
                .await
                .with_context(|| format!("fetching {clone_url}"))?;
            // git fetch never moves a bare mirror's HEAD — it stays
            // wherever clone set it. After a remote default-branch
            // rename, HEAD would dangle (--prune deleted the old ref)
            // or silently pin the old branch; re-derive it from the
            // remote on every fetch. Best-effort: the fetch itself
            // succeeded, and a HEAD left dangling fails the resolve
            // below loudly — a hiccup here must not fail a healthy
            // fetch.
            match remote_head(clone_url, token).await {
                // Only re-point at a branch this fetch actually brought:
                // one created-and-made-default after the fetch
                // enumerated refs would leave HEAD dangling until the
                // next sync fetches it.
                Ok(RemoteHead::Branch(branch)) => {
                    let target = format!("refs/heads/{branch}");
                    if git_says_yes(&local, &["rev-parse", "--verify", "--quiet", &target])
                        .await
                        .unwrap_or(false)
                        && let Err(e) =
                            run_git(Some(&local), &["symbolic-ref", "HEAD", &target], None).await
                    {
                        tracing::warn!(%clone_url, error = format!("{e:#}"), "could not re-point the mirror's HEAD; keeping the old one");
                    }
                }
                // The server hides the symref (old protocol, stripping
                // proxy) but still advertises HEAD's commit. A healthy
                // symref HEAD is left alone — never pinned to today's
                // tip — but a dangling one is detached at that commit,
                // and an already-detached one (a previous heal here) is
                // advanced to it, or the synced commit would freeze at
                // detach time forever. Probe failures err toward not
                // writing.
                Ok(RemoteHead::Commit(oid)) => {
                    let healthy_symref = git_says_yes(&local, &["symbolic-ref", "-q", "HEAD"])
                        .await
                        .unwrap_or(true)
                        && git_says_yes(
                            &local,
                            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
                        )
                        .await
                        .unwrap_or(true);
                    let have_commit =
                        git_says_yes(&local, &["cat-file", "-e", &format!("{oid}^{{commit}}")])
                            .await
                            .unwrap_or(false);
                    if !healthy_symref
                        && have_commit
                        && let Err(e) = run_git(
                            Some(&local),
                            &["update-ref", "--no-deref", "HEAD", &oid],
                            None,
                        )
                        .await
                    {
                        tracing::warn!(%clone_url, error = format!("{e:#}"), "could not point the mirror's HEAD at the remote's HEAD commit");
                    }
                }
                // An unborn or hidden remote HEAD: keep what we have.
                Ok(RemoteHead::Unknown) => {}
                Err(e) => {
                    tracing::warn!(%clone_url, error = format!("{e:#}"), "could not read the remote HEAD; keeping the mirror's");
                }
            }
        } else {
            remove_dir_if_present(&local)
                .await
                .context("clearing an unusable mirror from the git cache")?;
            tokio::fs::create_dir_all(&self.cache_dir)
                .await
                .context("creating the git cache directory")?;
            // Clone beside the final path, then rename: the real path
            // only ever holds a complete mirror, however the clone dies.
            // Each attempt gets its own partial dir (pid + counter), so
            // two workers whose leases overlapped never write into one
            // another's tree; the loser's rename fails and cleans up.
            static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let partial = self.cache_dir.join(format!(
                "{}.{}-{}",
                partial_prefix(repo_id),
                std::process::id(),
                ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let mut args: Vec<&str> = vec!["clone", "--bare", "--quiet"];
            args.extend(depth_arg.as_deref());
            let partial_str = partial.to_str().context("git cache path is not UTF-8")?;
            args.extend([clone_url, partial_str]);
            let cloned_into_place = async {
                run_git(None, &args, token)
                    .await
                    .with_context(|| format!("cloning {clone_url}"))?;
                tokio::fs::rename(&partial, &local)
                    .await
                    .context("moving the finished clone into place")
            }
            .await;
            if cloned_into_place.is_err() {
                let _ = remove_dir_if_present(&partial).await;
            }
            cloned_into_place?;
        }
        // --verify HEAD^{commit}: on a dangling HEAD, plain `rev-parse
        // HEAD` exits 0 and prints the literal string "HEAD" — which
        // would be recorded as the synced commit. Fail loudly instead.
        let head = run_git(
            Some(&local),
            &["rev-parse", "--verify", "HEAD^{commit}"],
            None,
        )
        .await
        .context("resolving the synced commit — the remote's HEAD may be unborn or dangling")?;
        Ok(head.trim().to_string())
    }
}

/// Whether a HEAD file plausibly names a ref ("ref: refs/…") or a
/// commit (a hex oid) — the two shapes git itself writes. A crash can
/// leave HEAD existing but NUL-filled or truncated (the journal
/// replays the rename without the data); such a mirror must re-clone,
/// not fail "not a git repository" on every retry forever. Judged on
/// bytes: refnames may legally hold non-UTF-8, and a healthy mirror
/// must never be condemned over its default branch's spelling.
fn head_names_a_ref(path: &std::path::Path) -> bool {
    let Ok(head) = std::fs::read(path) else {
        return false; // unreadable: wreckage
    };
    let head = head.trim_ascii_end();
    head.starts_with(b"ref: refs/")
        || (head.len() >= 40 && head.iter().all(|b| b.is_ascii_hexdigit()))
}

/// Where a remote's HEAD points, as `ls-remote --symref` advertises it.
enum RemoteHead {
    /// The default branch, from the symref capability.
    Branch(String),
    /// The server hid the symref but still listed HEAD's commit.
    Commit(String),
    /// No HEAD advertised at all (unborn HEAD, empty repo).
    Unknown,
}

async fn remote_head(clone_url: &str, token: Option<&str>) -> anyhow::Result<RemoteHead> {
    let out = run_git(None, &["ls-remote", "--symref", clone_url, "HEAD"], token)
        .await
        .with_context(|| format!("asking {clone_url} where its HEAD points"))?;
    let symref = out.lines().find_map(|line| {
        let target = line.strip_prefix("ref: ")?.strip_suffix("\tHEAD")?;
        Some(target.strip_prefix("refs/heads/")?.to_string())
    });
    if let Some(branch) = symref {
        return Ok(RemoteHead::Branch(branch));
    }
    let oid = out.lines().find_map(|line| {
        let oid = line.strip_suffix("\tHEAD")?;
        (oid.len() >= 40 && oid.bytes().all(|b| b.is_ascii_hexdigit())).then(|| oid.to_string())
    });
    Ok(oid.map_or(RemoteHead::Unknown, RemoteHead::Commit))
}

/// Run git for a yes/no question, `Ok` only when git actually ran. A
/// git that could not run (missing binary, spawn pressure, timeout) is
/// an `Err`, never a "no" — callers must not let an environmental blip
/// answer a question about repository state. `--git-dir` for the same
/// no-discovery reason as [`run_git`].
async fn git_says_yes(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<bool> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir").arg(dir);
    cmd.args(args);
    cmd.kill_on_drop(true);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    let status = tokio::time::timeout(GIT_TIMEOUT, cmd.status())
        .await
        .map_err(|_| anyhow::anyhow!("git {} timed out", args.first().unwrap_or(&"?")))?
        .context("running git (is it installed on this worker?)")?;
    Ok(status.success())
}

/// Holds one repo's mirror lock; work on the mirror — populating it
/// *and* reading it — happens only under a live guard.
pub struct MirrorGuard {
    /// The OS releases the advisory lock when the file closes — guard
    /// drop and worker crash alike.
    _lock_file: std::fs::File,
    _serialize_in_process: tokio::sync::OwnedMutexGuard<()>,
}

/// Serializes work on one repo's mirror — populating it *and* reading
/// it (`git archive` mid-fetch sees half a mirror). The per-kind job
/// leases don't prevent a fetch job and an index job from running
/// concurrently on one repo, in one process or in two sharing a cache
/// dir; two layers close both: an in-process mutex, then an advisory
/// file lock beside the mirror. Advisory locks are unreliable on
/// network filesystems — give a shared cache a local disk.
///
/// Acquisition is bounded by `timeout` — callers pass their job's
/// lease, past which a completion would be fenced off anyway. A hung
/// holder (a stuck git in another process) then fails this one job
/// into backoff instead of wedging the worker's whole queue behind an
/// unbounded wait.
pub async fn lock_mirror(
    cache_dir: &std::path::Path,
    repo_id: i64,
    timeout: Duration,
) -> anyhow::Result<MirrorGuard> {
    let started = std::time::Instant::now();
    // In-process contenders queue on the mutex, so at most one task per
    // process polls the file lock below.
    let in_process = tokio::time::timeout(timeout, mirror_mutex(repo_id).lock_owned())
        .await
        .map_err(|_| {
            anyhow::anyhow!("timed out waiting for this process's work on the repo's mirror")
        })?;
    let in_process_wait = started.elapsed();
    let remaining = timeout.saturating_sub(in_process_wait);
    let lock_path = cache_dir.join(format!("{repo_id}.git.lock"));
    let cache_dir = cache_dir.to_path_buf();
    let lock_file = tokio::task::spawn_blocking(move || -> anyhow::Result<std::fs::File> {
        std::fs::create_dir_all(&cache_dir).context("creating the git cache directory")?;
        let file = open_lock_file(&lock_path)?;
        let file_wait_started = std::time::Instant::now();
        let deadline = file_wait_started + remaining;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "gave up on the mirror lock {} after queueing in-process for \
                             {}s and waiting {}s on another process — often a long cold \
                             clone that will still land the mirror; retrying after backoff",
                            lock_path.display(),
                            in_process_wait.as_secs(),
                            file_wait_started.elapsed().as_secs()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| {
                        format!("locking the mirror lock {}", lock_path.display())
                    });
                }
            }
        }
    })
    .await
    .context("mirror lock task panicked")??;
    Ok(MirrorGuard {
        _lock_file: lock_file,
        _serialize_in_process: in_process,
    })
}

/// Open (creating if needed) a mirror lock file. A stray *directory*
/// squatting on the path is discarded and the open retried once: like
/// the mirrors beside it, the lock heals rather than failing the
/// repo's every job forever. Plain files are never unlinked, however
/// the open failed — unlink-and-recreate would split the lock across
/// two inodes, with the old file's holder and the new file's holder
/// each believing they own the mirror. (A healthy lock file always
/// opens — advisory locks don't block opens — so this can only cost us
/// healing exotic file wreckage, which stays a visible per-job error.)
fn open_lock_file(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
    let open = |path: &std::path::Path| {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false) // the file carries no content, only the lock
            .write(true)
            .open(path)
    };
    open(path).or_else(|_| {
        let _ = std::fs::remove_dir_all(path);
        open(path).with_context(|| format!("opening the mirror lock {}", path.display()))
    })
}

/// The in-process layer of [`lock_mirror`]. Entries are tiny and live
/// for the process — a registry of every repo this worker ever touched.
fn mirror_mutex(repo_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    LOCKS
        .lock()
        .expect("mirror lock registry poisoned")
        .entry(repo_id)
        .or_default()
        .clone()
}

/// Name prefix of repo `repo_id`'s in-progress clone attempts — minted
/// by the cloning path, matched by the sweep.
fn partial_prefix(repo_id: i64) -> String {
    format!("{repo_id}.git.partial")
}

/// Best-effort removal of wreckage from clone attempts that never made
/// it into place — crashed workers and rename-race losers leave
/// `<repo>.git.partial.*` directories behind. Callers run under the
/// repo's [`lock_mirror`] guard, which keeps the live clones of every
/// lock-taking process out of the sweep; a writer that bypasses the
/// lock (a pre-upgrade binary mid-rolling-deploy, a cache on a
/// filesystem whose advisory locks are no-ops, manual git) can still
/// lose its in-flight clone here — it fails, is fenced, and retries.
async fn sweep_stale_partials(cache_dir: &std::path::Path, repo_id: i64) {
    let prefix = partial_prefix(repo_id);
    let Ok(mut entries) = tokio::fs::read_dir(cache_dir).await else {
        return; // no cache dir yet — nothing to sweep
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = remove_dir_if_present(&entry.path()).await;
        }
    }
}

/// Clear whatever squats on a cache path — directory tree, plain file,
/// or nothing. "Already gone" is success: two workers whose leases
/// overlapped may race to clear the same wreckage, and losing that race
/// is fine.
async fn remove_dir_if_present(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::ErrorKind::{NotADirectory, NotFound};
    match tokio::fs::remove_dir_all(path).await {
        Err(e) if e.kind() == NotFound => Ok(()),
        Err(e) if e.kind() == NotADirectory => match tokio::fs::remove_file(path).await {
            Err(e) if e.kind() == NotFound => Ok(()),
            result => result,
        },
        result => result,
    }
}

/// Last-resort cap on a single git invocation. Network stalls — the
/// realistic hang, a blackholed connection that never RSTs — are
/// killed within ~a minute by the low-speed guard in [`run_git`]; this
/// backstop only fires on non-network hangs (a dead cache filesystem),
/// so it is sized to never kill a legitimately slow cold clone of a
/// huge repo. Such a clone outlives its job's lease, but it still
/// lands the mirror — the re-claimed job then needs only a cheap
/// fetch. The timeout drops the future and `kill_on_drop` reaps git.
const GIT_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// Run git non-interactively, returning stdout. The Forge token travels
/// via `GIT_CONFIG_*` environment variables — never the command line
/// (visible in `ps`) and never the on-disk config.
///
/// `dir` is passed as `--git-dir`, not `-C`: `-C` *discovers* the
/// repository, climbing parent directories when `dir` isn't one — a
/// torn mirror inside some enclosing checkout would have its destructive
/// fetch run against that checkout instead of failing.
async fn run_git(
    dir: Option<&std::path::Path>,
    args: &[&str],
    token: Option<&str>,
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("--git-dir").arg(dir);
    }
    cmd.args(args);
    // If this future is dropped (shutdown, lease handling), take the git
    // process down with it instead of orphaning a half-done clone.
    cmd.kill_on_drop(true);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // Transfers that stall — under 1 KB/s for a minute straight — are
    // dead connections, not slow ones; have git kill them itself so a
    // blackholed remote fails the job in ~a minute instead of holding
    // the worker loop and the mirror lock until GIT_TIMEOUT.
    let mut config = vec![
        ("http.lowSpeedLimit", "1024".to_string()),
        ("http.lowSpeedTime", "60".to_string()),
    ];
    if let Some(token) = token {
        // GitHub accepts any username with the token as password.
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        config.push(("http.extraHeader", format!("Authorization: Basic {basic}")));
    }
    cmd.env("GIT_CONFIG_COUNT", config.len().to_string());
    for (i, (key, value)) in config.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), key);
        cmd.env(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "git {} still running after {} hours; killed (hung filesystem?)",
                args.first().unwrap_or(&"?"),
                GIT_TIMEOUT.as_secs() / 3600
            )
        })?
        .context("running git (is it installed on this worker?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&"?"),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Where a repository lives, split the way the control plane stores it:
/// a Forge (`base_url`) plus a repo path on it (`slug`). The clone URL is
/// re-derived as `{base_url}/{slug}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLocator {
    pub kind: ForgeKind,
    /// Forge root, e.g. `https://github.com` — unique key for the forge.
    pub base_url: String,
    /// Repo path on the forge, e.g. `acme/widgets`.
    pub slug: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeKind {
    Github,
    /// Any other git remote (file:// fixtures, self-hosted mirrors).
    Git,
}

impl ForgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ForgeKind::Github => "github",
            ForgeKind::Git => "git",
        }
    }

    /// Default environment variable this Forge kind's token is read
    /// from. Only the default at registration: `forges.token_env` in the
    /// control plane is what workers actually consult at fetch time, so
    /// a per-forge override there wins.
    pub fn token_env(self) -> Option<&'static str> {
        match self {
            ForgeKind::Github => Some("YG_GITHUB_TOKEN"),
            ForgeKind::Git => None,
        }
    }
}

impl RepoLocator {
    /// Parse a repository URL as given to `yg admin repo add`.
    ///
    /// `https://github.com/acme/widgets` → github forge, slug
    /// `acme/widgets`. Nested paths (GitLab groups) keep the full path as
    /// the slug. `file://` URLs (test fixtures, local mirrors) treat the
    /// last two path segments as the slug and the rest as the forge root.
    ///
    /// Cosmetic variation normalizes away (`.git` suffix, trailing or
    /// doubled slashes, host case), so every spelling of a repo lands on
    /// the same forge + slug. Anything that isn't plainly a repository
    /// path — credentials, query strings, fragments, `.`/`..` segments —
    /// is rejected rather than guessed at.
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim().trim_end_matches('/');
        if url.contains('?') || url.contains('#') {
            return Err(format!(
                "repository URLs don't take query strings or fragments: {url}"
            ));
        }
        let stripped = url.strip_suffix(".git").unwrap_or(url);

        if let Some(path) = stripped.strip_prefix("file://") {
            // file:///abs/path only — a `file://host/…` authority would
            // silently become a path component.
            if !path.starts_with('/') {
                return Err(format!(
                    "file URLs must be absolute (file:///path/to/repo): {url}"
                ));
            }
            let segments = path_segments(path)?;
            let Some((base_parts, slug_parts)) = segments.split_last_chunk::<2>() else {
                return Err(format!(
                    "file URL needs at least two path segments (owner/repo): {url}"
                ));
            };
            return Ok(Self {
                kind: ForgeKind::Git,
                base_url: format!("file:///{}", base_parts.join("/")),
                slug: slug_parts.join("/"),
            });
        }

        let (scheme, rest) = stripped
            .split_once("://")
            .ok_or_else(|| format!("not a repository URL (expected scheme://…): {url}"))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && scheme != "http" {
            return Err(format!("unsupported URL scheme {scheme:?}: {url}"));
        }
        let (host, path) = rest
            .split_once('/')
            .ok_or_else(|| format!("repository URL has no path: {url}"))?;
        if host.is_empty() {
            return Err(format!("repository URL has no host: {url}"));
        }
        if host.contains('@') {
            return Err(format!(
                "credentials in repository URLs are not accepted \
                 (the worker reads tokens from the Forge's environment variable): {url}"
            ));
        }
        // DNS is case-insensitive; normalize so URL spelling can't split
        // one forge into several.
        let host = host.to_ascii_lowercase();
        let segments = path_segments(path)?;
        if segments.len() < 2 {
            return Err(format!(
                "repository path must be at least owner/repo: {url}"
            ));
        }
        let kind = if host == "github.com" {
            ForgeKind::Github
        } else {
            ForgeKind::Git
        };
        // GitHub repos live at exactly owner/repo; a longer path is a
        // pasted browser page (tree/…, issues/…), not a different repo —
        // rejected rather than guessed at.
        if kind == ForgeKind::Github && segments.len() > 2 {
            return Err(format!(
                "GitHub repositories are owner/repo — drop the trailing path \
                 (got {} extra segment(s)): {url}",
                segments.len() - 2
            ));
        }
        // GitHub only speaks https; normalizing here keeps a worker from
        // ever sending the Forge token over plaintext because of a URL
        // spelling, and keeps http/https variants on one forge row.
        let scheme = if kind == ForgeKind::Github {
            "https".to_string()
        } else {
            scheme
        };
        Ok(Self {
            kind,
            base_url: format!("{scheme}://{host}"),
            slug: segments.join("/"),
        })
    }

    /// The URL workers clone/fetch from.
    pub fn clone_url(&self) -> String {
        join_clone_url(&self.base_url, &self.slug)
    }
}

/// The single derivation of a clone URL from its stored halves — used by
/// [`RepoLocator::clone_url`] and the worker re-deriving it from a claim.
/// Strips one trailing slash from the base so the degenerate `file:///`
/// forge root doesn't join into a doubled slash.
pub fn join_clone_url(base_url: &str, slug: &str) -> String {
    let base = base_url.strip_suffix('/').unwrap_or(base_url);
    format!("{base}/{slug}")
}

/// A repository path split into its meaningful segments: empty segments
/// (doubled slashes) collapse; `.`/`..` segments are rejected — they
/// never name a repository, only an escape attempt or a typo.
fn path_segments(path: &str) -> Result<Vec<&str>, String> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.iter().any(|s| *s == "." || *s == "..") {
        return Err(format!(
            "repository paths must not contain '.' or '..' segments: {path}"
        ));
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(url: &str) -> RepoLocator {
        RepoLocator::parse(url).expect(url)
    }

    #[test]
    fn github_urls_split_into_forge_root_and_slug() {
        let locator = parsed("https://github.com/acme/widgets");
        assert_eq!(locator.kind, ForgeKind::Github);
        assert_eq!(locator.base_url, "https://github.com");
        assert_eq!(locator.slug, "acme/widgets");
        assert_eq!(locator.clone_url(), "https://github.com/acme/widgets");
    }

    #[test]
    fn cosmetic_variants_normalize_to_the_same_repo() {
        let canonical = parsed("https://github.com/acme/widgets");
        for variant in [
            "https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets/",
            "  https://github.com/acme/widgets ",
            "https://github.com//acme//widgets",
            "https://GITHUB.COM/acme/widgets",
        ] {
            let locator = parsed(variant);
            assert_eq!(locator.slug, canonical.slug, "{variant}");
            assert_eq!(
                locator.base_url, canonical.base_url,
                "{variant} must land on the same forge row"
            );
        }
    }

    #[test]
    fn nested_group_paths_keep_the_full_path_as_slug() {
        assert_eq!(parsed("https://gitlab.example/a/b/c").slug, "a/b/c");
    }

    #[test]
    fn github_subpage_urls_are_rejected_not_guessed_at() {
        // Pasted browser URLs: the repo is owner/repo, the rest is a page.
        for url in [
            "https://github.com/acme/widgets/tree/main",
            "https://github.com/acme/widgets/issues/5",
            "https://github.com/acme/widgets/blob/main/README.md",
        ] {
            let err = RepoLocator::parse(url).unwrap_err();
            assert!(err.contains("owner/repo"), "{url} → {err}");
        }
    }

    #[test]
    fn github_over_plain_http_normalizes_to_https() {
        // The GitHub forge always speaks https; a worker must never send
        // its token over plaintext because of a URL spelling.
        let locator = parsed("http://github.com/acme/widgets");
        assert_eq!(locator.kind, ForgeKind::Github);
        assert_eq!(locator.base_url, "https://github.com");
        assert_eq!(
            locator.base_url,
            parsed("https://github.com/acme/widgets").base_url,
            "http and https spellings must land on the same forge row"
        );
    }

    #[test]
    fn file_urls_use_the_last_two_segments_as_slug() {
        let locator = parsed("file:///tmp/fixtures/acme/widgets");
        assert_eq!(locator.kind, ForgeKind::Git);
        assert_eq!(locator.base_url, "file:///tmp/fixtures");
        assert_eq!(locator.slug, "acme/widgets");
        assert_eq!(locator.clone_url(), "file:///tmp/fixtures/acme/widgets");
    }

    #[test]
    fn a_repo_at_the_filesystem_root_round_trips_to_a_clean_clone_url() {
        // Degenerate but legal: the forge root collapses to file:///.
        let locator = parsed("file:///acme/widgets");
        assert_eq!(locator.slug, "acme/widgets");
        assert_eq!(
            locator.clone_url(),
            "file:///acme/widgets",
            "joining must not double the slash after a bare file:/// root"
        );
    }

    #[test]
    fn urls_carrying_credentials_are_rejected() {
        for url in [
            "https://user:pass@github.com/acme/widgets",
            "https://token@github.com/acme/widgets",
        ] {
            let err = RepoLocator::parse(url).unwrap_err();
            assert!(err.contains("credentials"), "{url} → {err}");
        }
    }

    #[test]
    fn urls_with_query_strings_or_fragments_are_rejected() {
        for url in [
            "https://github.com/acme/widgets?ref=main",
            "https://github.com/acme/widgets#readme",
            "file:///tmp/fixtures/acme/widgets?x=1",
        ] {
            assert!(RepoLocator::parse(url).is_err(), "{url} must be rejected");
        }
    }

    #[test]
    fn dot_segments_are_rejected() {
        for url in [
            "https://github.com/acme/..",
            "https://github.com/acme/../evil/widgets",
            "https://github.com/acme/./widgets",
            "file:///tmp/fixtures/../escape/acme/widgets",
        ] {
            assert!(RepoLocator::parse(url).is_err(), "{url} must be rejected");
        }
    }

    #[test]
    fn urls_that_are_not_repositories_are_rejected() {
        for url in [
            "not a url",
            "ssh://github.com/acme/widgets",
            "https://github.com/acme",
            "https://github.com",
            "https:///acme/widgets",
            "file:///lonely",
            "file://somehost/tmp/acme/widgets", // authority, not a path
        ] {
            assert!(RepoLocator::parse(url).is_err(), "{url} must be rejected");
        }
    }
}
