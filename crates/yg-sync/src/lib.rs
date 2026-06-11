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
        let token = job
            .token_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok())
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            // Defense in depth: whatever the control plane says, a Forge
            // token only ever travels over TLS.
            .filter(|_| clone_url.starts_with("https://"));
        match self
            .fetcher
            .sync(job.repo_id, &clone_url, token.as_deref(), job.fetch_depth)
            .await
        {
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

/// Mirrors remote repositories into a local cache of bare clones, one
/// `<repo-id>.git` per repo.
struct GitFetcher {
    cache_dir: PathBuf,
}

impl GitFetcher {
    fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    /// Bare-clone `clone_url` on first sight, fetch it afterwards; either
    /// way the cache ends at the remote's current state. Returns the
    /// commit the remote's default branch points at.
    ///
    /// A mirror that exists but isn't usable (interrupted clone, stray
    /// deletion) is discarded and re-cloned rather than left to fail
    /// every retry forever.
    async fn sync(
        &self,
        repo_id: i64,
        clone_url: &str,
        token: Option<&str>,
        depth: Option<i32>,
    ) -> anyhow::Result<String> {
        let local = self.cache_dir.join(format!("{repo_id}.git"));
        let depth_arg = depth.map(|n| format!("--depth={n}"));
        sweep_stale_partials(&self.cache_dir, repo_id).await;
        // Every bare repo has a HEAD file; a directory without one is
        // wreckage, not a mirror.
        let usable = local.join("HEAD").exists();
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
                "{repo_id}.git.partial.{}-{}",
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
        let head = run_git(Some(&local), &["rev-parse", "HEAD"], None)
            .await
            .context("resolving the synced commit")?;
        Ok(head.trim().to_string())
    }
}

/// Best-effort removal of wreckage from clone attempts that never made
/// it into place — crashed workers and rename-race losers leave
/// `<repo>.git.partial.*` directories behind. Sweeping may also kill a
/// clone another worker is running right now, but only when both hold
/// the same repo (an expired-lease overlap): that worker fails, its
/// result is fenced off anyway, and the queue retries.
async fn sweep_stale_partials(cache_dir: &std::path::Path, repo_id: i64) {
    let prefix = format!("{repo_id}.git.partial");
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

/// Run git non-interactively, returning stdout. The Forge token travels
/// via `GIT_CONFIG_*` environment variables — never the command line
/// (visible in `ps`) and never the on-disk config.
async fn run_git(
    dir: Option<&std::path::Path>,
    args: &[&str],
    token: Option<&str>,
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    // If this future is dropped (shutdown, lease handling), take the git
    // process down with it instead of orphaning a half-done clone.
    cmd.kill_on_drop(true);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(token) = token {
        // GitHub accepts any username with the token as password.
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        cmd.env("GIT_CONFIG_COUNT", "1");
        cmd.env("GIT_CONFIG_KEY_0", "http.extraHeader");
        cmd.env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Basic {basic}"),
        );
    }
    let out = cmd
        .output()
        .await
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
