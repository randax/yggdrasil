//! Git plumbing: the mirror cache, its locks, and non-interactive git.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;

use crate::forge::GitAuth;

/// Locale used for git's human-readable diagnostics, which rate-limit
/// detection subsequently matches as English prose.
const GIT_LOCALE: &str = "C";

/// Construct every git child with the process-wide non-interactive and
/// diagnostic-language contract applied in one place.
fn git_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("LC_ALL", GIT_LOCALE);
    command
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

/// The commit a remote's default branch (HEAD) points at, read with a
/// single `git ls-remote` — the cheap conditional request the poll loop
/// spends to detect a moved head without transferring objects. `None`
/// when the remote advertises no HEAD commit (an empty repo, an unborn
/// default branch).
pub async fn remote_head_commit(
    clone_url: &str,
    auth: Option<&GitAuth>,
) -> anyhow::Result<Option<String>> {
    let out = run_git(None, &["ls-remote", clone_url, "HEAD"], auth)
        .await
        .with_context(|| format!("polling {clone_url} for its head"))?;
    // ls-remote (no --symref) prints one line, `<sha>\tHEAD`; take the sha.
    let head = out.lines().find_map(|line| {
        let (oid, name) = line.split_once('\t')?;
        (name == "HEAD" && oid.len() >= 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| oid.to_string())
    });
    Ok(head)
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
        auth: Option<&GitAuth>,
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
            run_git(Some(&local), &args, auth)
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
            match remote_head(clone_url, auth).await {
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
                run_git(None, &args, auth)
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
    // One logical line, no embedded NULs: a partial write can splice
    // garbage after a valid-looking prefix, and accepting it would keep
    // reusing (and failing on) a mirror the re-clone path should heal.
    if head.iter().any(|b| matches!(b, 0 | b'\n' | b'\r')) {
        return false;
    }
    match head.strip_prefix(b"ref: refs/") {
        Some(target) => target.iter().any(|b| !b.is_ascii_whitespace()),
        // Detached form: exactly a sha1 or sha256 — git writes nothing
        // else, and an unbounded hex check would accept garbage blobs.
        None => {
            (head.len() == 40 || head.len() == 64) && head.iter().all(|b| b.is_ascii_hexdigit())
        }
    }
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

async fn remote_head(clone_url: &str, auth: Option<&GitAuth>) -> anyhow::Result<RemoteHead> {
    let out = run_git(None, &["ls-remote", "--symref", clone_url, "HEAD"], auth)
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
    let mut cmd = git_command("git");
    cmd.arg("--git-dir").arg(dir);
    cmd.args(args);
    cmd.kill_on_drop(true);
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
    let lock_task = tokio::task::spawn_blocking(move || -> anyhow::Result<std::fs::File> {
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
    });
    // The poll loop above bounds itself, but only once it is running: a
    // hung filesystem call before it (create_dir_all, the open) or a
    // saturated blocking pool would otherwise wedge this future — and
    // the in-process guard it holds — past every deadline. Bound the
    // whole blocking task by the same budget. On timeout the detached
    // task's eventual result is dropped, closing (and so releasing) a
    // lock it may still win.
    let lock_file = tokio::time::timeout(remaining, lock_task)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out before the mirror lock poll could run — a hung cache \
                 filesystem or a saturated blocking pool; retrying after backoff"
            )
        })?
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
    auth: Option<&GitAuth>,
) -> anyhow::Result<String> {
    let mut cmd = git_command("git");
    if let Some(dir) = dir {
        cmd.arg("--git-dir").arg(dir);
    }
    cmd.args(args);
    // If this future is dropped (shutdown, lease handling), take the git
    // process down with it instead of orphaning a half-done clone.
    cmd.kill_on_drop(true);
    // Transfers that stall — under 1 KB/s for a minute straight — are
    // dead connections, not slow ones; have git kill them itself so a
    // blackholed remote fails the job in ~a minute instead of holding
    // the worker loop and the mirror lock until GIT_TIMEOUT.
    let mut config = vec![
        ("http.lowSpeedLimit", "1024".to_string()),
        ("http.lowSpeedTime", "60".to_string()),
    ];
    if let Some(auth) = auth {
        // The forge adapter dictates the username its tokens pair with.
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", auth.username, auth.token));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn localized_git_stderr_still_exposes_an_english_rate_limit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("git");
        std::fs::write(
            &fixture,
            include_str!("../testdata/localized-git-stderr.sh"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fixture, permissions).unwrap();

        let output = git_command(&fixture).output().await.unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("The requested URL returned error: 429"));
        assert!(
            crate::forge::builtin()
                .for_kind("github")
                .is_rate_limit(&stderr),
            "C-locale git stderr must remain recognizable as a rate limit: {stderr:?}"
        );
    }

    #[test]
    fn head_validation_rejects_crash_artifacts_and_accepts_gits_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let head = dir.path().join("HEAD");
        let names_a_ref = |bytes: &[u8]| {
            std::fs::write(&head, bytes).unwrap();
            head_names_a_ref(&head)
        };
        // The shapes git writes.
        assert!(names_a_ref(b"ref: refs/heads/main\n"));
        assert!(
            names_a_ref(b"ref: refs/heads/caf\xe9\n"),
            "refnames may legally hold non-UTF-8"
        );
        assert!(
            names_a_ref(b"0123456789abcdef0123456789abcdef01234567\n"),
            "detached sha1"
        );
        assert!(
            names_a_ref(format!("{}\n", "a".repeat(64)).as_bytes()),
            "detached sha256"
        );
        // Crash artifacts and garbage.
        assert!(!names_a_ref(b""), "empty");
        assert!(!names_a_ref(b"\0\0\0\0\0"), "NUL-filled");
        assert!(
            !names_a_ref(b"ref: refs/heads/main\0\0\0"),
            "NULs spliced after a valid-looking prefix"
        );
        assert!(!names_a_ref(b"ref: refs/   "), "no target after the prefix");
        assert!(!names_a_ref(b"ref: re"), "truncated mid-prefix");
        assert!(
            !names_a_ref(b"ref: refs/heads/a\nref: refs/heads/b\n"),
            "more than one line"
        );
        assert!(
            !names_a_ref(b"0123456789abcdef\n"),
            "hex but not an oid length"
        );
        assert!(
            !names_a_ref("a".repeat(70).as_bytes()),
            "hex garbage block of non-oid length"
        );
        assert!(!names_a_ref(b"not a head at all"), "prose");
    }
}
