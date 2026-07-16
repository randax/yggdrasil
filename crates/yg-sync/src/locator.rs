//! Parsing repository URLs into a Forge root plus repo slug.

use crate::forge::Forge;
use crate::forge::ForgeRegistry;
use crate::forge::git_generic::GitForge;

/// Why a repository URL failed to parse. Typed per the repo's
/// error-modelling rule; rendered for humans (the admin API's
/// bad-request body) only at the I/O edge via `Display`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocatorError {
    #[error("repository URLs don't take query strings or fragments: {url}")]
    QueryOrFragment { url: String },
    #[error("file URLs must be absolute (file:///path/to/repo): {url}")]
    RelativeFileUrl { url: String },
    #[error("file URL needs at least two path segments (owner/repo): {url}")]
    TooFewFileSegments { url: String },
    #[error("not a repository URL (expected scheme://…): {url}")]
    NotAUrl { url: String },
    #[error("unsupported URL scheme {scheme:?}: {url}")]
    UnsupportedScheme { scheme: String, url: String },
    #[error("repository URL has no path: {url}")]
    NoPath { url: String },
    #[error("repository URL has no host: {url}")]
    NoHost { url: String },
    #[error(
        "credentials in repository URLs are not accepted \
         (the worker reads tokens from the Forge's environment variable): {url}"
    )]
    CredentialsInUrl { url: String },
    #[error("repository path must be at least owner/repo: {url}")]
    TooFewSegments { url: String },
    #[error("repository paths must not contain '.' or '..' segments: {path}")]
    DotSegments { path: String },
    #[error("repository paths must not contain whitespace or control characters: {path}")]
    ForbiddenPathCharacters { path: String },
    #[error(
        "GitHub repositories are owner/repo — drop the trailing path \
         (got {extra} extra segment(s)): {url}"
    )]
    GitHubSubpageUrl { extra: usize, url: String },
}

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

/// A syntactically parsed repository URL whose Forge adapter has not yet been
/// selected. Keeping this phase separate lets callers consult configured
/// Forge records before falling back to host claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnclassifiedRepoLocator {
    url: String,
    scheme: Option<String>,
    host: Option<String>,
    base_url: String,
    segments: Vec<String>,
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
    pub fn parse(url: &str) -> Result<Self, LocatorError> {
        Self::parse_with_registry(url, crate::forge::builtin())
    }

    /// Parse using an injected Forge registry for host classification.
    pub fn parse_with_registry(url: &str, registry: &ForgeRegistry) -> Result<Self, LocatorError> {
        Self::parse_unclassified(url)?.resolve_with_registry(registry)
    }

    /// Parse URL structure without selecting a Forge adapter.
    pub fn parse_unclassified(url: &str) -> Result<UnclassifiedRepoLocator, LocatorError> {
        let url = url.trim().trim_end_matches('/');
        if url.contains('?') || url.contains('#') {
            return Err(LocatorError::QueryOrFragment { url: url.into() });
        }
        let stripped = url.strip_suffix(".git").unwrap_or(url);

        if let Some(path) = stripped.strip_prefix("file://") {
            // file:///abs/path only — a `file://host/…` authority would
            // silently become a path component.
            if !path.starts_with('/') {
                return Err(LocatorError::RelativeFileUrl { url: url.into() });
            }
            let segments = path_segments(path)?;
            let Some((base_parts, slug_parts)) = segments.split_last_chunk::<2>() else {
                return Err(LocatorError::TooFewFileSegments { url: url.into() });
            };
            return Ok(UnclassifiedRepoLocator {
                url: url.into(),
                scheme: None,
                host: None,
                base_url: format!("file:///{}", base_parts.join("/")),
                segments: slug_parts.iter().map(|segment| (*segment).into()).collect(),
            });
        }

        let (scheme, rest) = stripped
            .split_once("://")
            .ok_or_else(|| LocatorError::NotAUrl { url: url.into() })?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && scheme != "http" {
            return Err(LocatorError::UnsupportedScheme {
                scheme,
                url: url.into(),
            });
        }
        let (host, path) = rest
            .split_once('/')
            .ok_or_else(|| LocatorError::NoPath { url: url.into() })?;
        if host.is_empty() {
            return Err(LocatorError::NoHost { url: url.into() });
        }
        if host.contains('@') {
            return Err(LocatorError::CredentialsInUrl { url: url.into() });
        }
        // DNS is case-insensitive; normalize so URL spelling can't split
        // one forge into several.
        let host = host.to_ascii_lowercase();
        let segments = path_segments(path)?;
        if segments.len() < 2 {
            return Err(LocatorError::TooFewSegments { url: url.into() });
        }
        Ok(UnclassifiedRepoLocator {
            url: url.into(),
            base_url: format!("{scheme}://{host}"),
            scheme: Some(scheme),
            host: Some(host),
            segments: segments.into_iter().map(Into::into).collect(),
        })
    }

    /// The URL workers clone/fetch from.
    pub fn clone_url(&self) -> String {
        join_clone_url(&self.base_url, &self.slug)
    }
}

impl UnclassifiedRepoLocator {
    /// The canonical root spelling used only to query configured Forge records.
    ///
    /// Forge administration stores HTTP-capable Forge roots as HTTPS. Preserve
    /// the typed scheme on this locator so an unknown HTTP Git remote still
    /// reaches the generic fallback unchanged.
    pub fn configured_lookup_base_url(
        &self,
    ) -> Result<yg_control::ForgeUrl, yg_control::ForgeUrlParseError> {
        let base_url = match (&self.scheme, &self.host) {
            (Some(scheme), Some(host)) if scheme == "http" => format!("https://{host}"),
            _ => self.base_url.clone(),
        };
        yg_control::ForgeUrl::parse(base_url)
    }

    /// Resolve with the adapter selected by a configured Forge record.
    ///
    /// The adapter sees the canonical HTTPS spelling used for the lookup, and
    /// the resulting locator retains the exact configured root. The original
    /// typed scheme remains untouched on the fallback path.
    pub fn resolve_configured(
        mut self,
        forge: &dyn Forge,
        configured_base_url: &yg_control::ForgeUrl,
    ) -> Result<RepoLocator, LocatorError> {
        if self.scheme.as_deref() == Some("http") {
            self.scheme = Some("https".into());
        }
        let mut locator = self.resolve(forge)?;
        locator.base_url = configured_base_url.as_str().to_owned();
        Ok(locator)
    }

    /// Resolve through host claims when no configured Forge record matched.
    pub fn resolve_with_registry(
        self,
        registry: &ForgeRegistry,
    ) -> Result<RepoLocator, LocatorError> {
        let forge = self
            .host
            .as_deref()
            .map_or(&GitForge as &dyn Forge, |host| registry.for_host(host));
        self.resolve(forge)
    }

    /// Resolve with an explicitly selected adapter.
    fn resolve(self, forge: &dyn Forge) -> Result<RepoLocator, LocatorError> {
        let base_url = match (&self.scheme, &self.host) {
            (Some(scheme), Some(host)) => {
                let segments: Vec<&str> = self.segments.iter().map(String::as_str).collect();
                let scheme = forge.canonical_scheme(scheme, &segments, &self.url)?;
                format!("{scheme}://{host}")
            }
            (None, None) => self.base_url,
            _ => unreachable!("parsed locators have both a scheme and host, or neither"),
        };
        Ok(RepoLocator {
            kind: forge.kind(),
            base_url,
            slug: self.segments.join("/"),
        })
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
fn path_segments(path: &str) -> Result<Vec<&str>, LocatorError> {
    // Whitespace and control characters never appear in a real
    // repository path; stored in the slug they would be rejoined
    // verbatim into a clone URL that fails every fetch forever.
    if path
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(LocatorError::ForbiddenPathCharacters { path: path.into() });
    }
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.iter().any(|s| *s == "." || *s == "..") {
        return Err(LocatorError::DotSegments { path: path.into() });
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
            assert!(err.to_string().contains("owner/repo"), "{url} → {err}");
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
            assert!(
                matches!(err, LocatorError::CredentialsInUrl { .. }),
                "{url} → {err}"
            );
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
    fn paths_with_whitespace_or_control_characters_are_rejected() {
        for url in [
            "https://gitlab.example/acme/my repo",
            "https://gitlab.example/acme/wid\tgets",
            "file:///tmp/fixtures/acme/my repo",
        ] {
            let err = RepoLocator::parse(url).unwrap_err();
            assert!(
                matches!(err, LocatorError::ForbiddenPathCharacters { .. }),
                "{url} → {err}"
            );
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
