//! Parsing repository URLs into a Forge root plus repo slug.

use crate::forge::Forge;
use crate::forge::git_generic::GitForge;

/// Where a repository lives, split the way the control plane stores it:
/// a Forge (`base_url`) plus a repo path on it (`slug`). The clone URL is
/// re-derived as `{base_url}/{slug}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLocator {
    /// The forge adapter's kind string, e.g. `github`, or `git` for any
    /// other remote — resolved by the [`crate::forge::ForgeRegistry`].
    pub kind: &'static str,
    /// Forge root, e.g. `https://github.com` — unique key for the forge.
    pub base_url: String,
    /// Repo path on the forge, e.g. `acme/widgets`.
    pub slug: String,
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
                kind: GitForge.kind(),
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
        // The forge claiming this host owns the rest of the rules: path
        // shape (GitHub is exactly owner/repo) and canonical scheme
        // (GitHub only speaks https, so a token never travels plaintext
        // because of a URL spelling and http/https variants land on one
        // forge row).
        let forge = crate::forge::builtin().for_host(&host);
        let scheme = forge.canonical_repo_url(&scheme, &segments, url)?;
        Ok(Self {
            kind: forge.kind(),
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
        assert_eq!(locator.kind, "github");
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
        assert_eq!(locator.kind, "github");
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
        assert_eq!(locator.kind, "git");
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
