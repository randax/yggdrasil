//! The Forge seam: everything the sync loops need from a code host,
//! behind one trait. Adding a forge is one [`Forge`] implementation
//! plus registration in a [`ForgeRegistry`]; nothing outside the
//! adapters compares forge kinds.

pub(crate) mod git_generic;
pub(crate) mod github;

pub use github::GitHubListingError;

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// A Forge API response asking this process to stop making requests for a
/// bounded period. Adapters surface this typed signal so the worker can apply
/// it to the same per-Forge cooldown used by polling.
#[derive(Debug, thiserror::Error)]
#[error("forge returned {status} and requested a {retry_after:?} cooldown")]
pub struct ForgeRateLimit {
    status: reqwest::StatusCode,
    retry_after: Duration,
}

impl ForgeRateLimit {
    /// Create a Forge-wide cooldown signal for an API response.
    pub fn new(status: reqwest::StatusCode, retry_after: Duration) -> Self {
        Self {
            status,
            retry_after,
        }
    }

    /// The duration the Forge-wide request bucket must remain paused.
    pub fn retry_after(&self) -> Duration {
        self.retry_after
    }
}

/// Boxed future for dyn-compatible async trait methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One repository as a Forge's discovery listing returns it.
#[derive(Debug)]
pub struct ListedRepo {
    pub slug: String,
    pub visibility: yg_control::RepoVisibility,
}

/// A Forge request could not be spent from the worker's per-process
/// budget. The duration says when the budget can next grant a request.
#[derive(Debug, thiserror::Error)]
#[error("Forge request budget exhausted; retry after {retry_after:?}")]
pub struct ForgeBudgetExhausted {
    pub retry_after: Duration,
}

/// The per-process request budget presented to a Forge discovery
/// adapter. Adapters take one token immediately before each API call.
pub trait ForgeRequestBudget: Send + Sync {
    fn take(&self) -> Result<(), ForgeBudgetExhausted>;
}

/// Wait until one Forge request can be charged. Sleeping is cancellable with
/// the surrounding discovery future, so shutdown never leaves detached work.
pub(crate) async fn acquire_forge_request(budget: &dyn ForgeRequestBudget) {
    loop {
        match budget.take() {
            Ok(()) => return,
            Err(exhausted) => tokio::time::sleep(exhausted.retry_after).await,
        }
    }
}

/// Basic-auth credentials for git-over-HTTP: each forge dictates the
/// username its tokens pair with (GitHub accepts any username and
/// conventionally `x-access-token`; GitLab would require `oauth2`).
pub struct GitAuth {
    pub username: &'static str,
    pub token: String,
}

/// A code host the sync loops can work against. Everything
/// forge-specific — how its URLs look, how its API is found, how its
/// tokens authenticate a clone, how it signals rate limits, and how its
/// org repositories are listed — resolves through this trait.
pub trait Forge: Send + Sync {
    /// The kind string this adapter registers under — the value stored
    /// in `forges.kind`. Compared only inside [`ForgeRegistry`].
    fn kind(&self) -> &'static str;

    /// Whether repository URLs on `host` belong to this forge.
    fn claims_host(&self, host: &str) -> bool;

    /// Default env var this forge's token is read from. Only the
    /// default at registration: `forges.token_env` in the control plane
    /// is what workers actually consult at fetch time, so a per-forge
    /// override there wins.
    fn default_token_env(&self) -> Option<&'static str>;

    /// The REST API root recorded on the Forge record when a forge at
    /// `base_url` is registered, for forges that have an API.
    fn default_api_root(&self, base_url: &str) -> Option<String>;

    /// Forge-specific repository-URL rules, applied after the generic
    /// parse: validate the path shape and return the canonical scheme.
    /// The default accepts any path and keeps the scheme.
    fn canonical_scheme(
        &self,
        scheme: &str,
        segments: &[&str],
        url: &str,
    ) -> Result<String, crate::locator::LocatorError> {
        let _ = (segments, url);
        Ok(scheme.to_string())
    }

    /// Basic-auth credentials a clone/fetch presents for `token`. The
    /// default is the `x-access-token` username most git hosts accept;
    /// a forge that requires its own (GitLab's `oauth2`) overrides.
    fn git_auth(&self, token: String) -> GitAuth {
        GitAuth {
            username: "x-access-token",
            token,
        }
    }

    /// Whether a git failure message is this forge pushing back on
    /// request volume (rate limit, abuse detection) rather than an
    /// ordinary error (missing repo, auth, DNS).
    fn is_rate_limit(&self, message: &str) -> bool;

    /// Org repository discovery, for forges whose API can list an
    /// org's repositories. `None` for plain git remotes.
    fn discovery(&self) -> Option<&dyn OrgDiscovery>;
}

/// Listing an org's repositories through a forge's API.
pub trait OrgDiscovery: Send + Sync {
    /// List every repository of `org`, following pagination. Return
    /// [`ForgeRateLimit`] to request a Forge-wide cooldown.
    fn list_org_repos<'a>(
        &'a self,
        client: &'a reqwest::Client,
        api_root: &'a str,
        org: &'a str,
        token: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>>;

    /// List every repository while charging each discovery operation to
    /// the worker's Forge request budget. The default covers adapters
    /// whose listing is one request; paginated adapters override this
    /// and take a token before every page. Return [`ForgeRateLimit`] to
    /// request a Forge-wide cooldown.
    fn list_org_repos_budgeted<'a>(
        &'a self,
        client: &'a reqwest::Client,
        api_root: &'a str,
        org: &'a str,
        token: Option<&'a str>,
        budget: &'a dyn ForgeRequestBudget,
    ) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>> {
        Box::pin(async move {
            acquire_forge_request(budget).await;
            self.list_org_repos(client, api_root, org, token).await
        })
    }
}

/// The forge adapters a worker dispatches through, looked up by kind
/// (claims carry the forge kind) or by host (URL registration). The
/// generic git adapter claims every host, so it terminates `for_host`;
/// custom adapters register ahead of the built-ins.
#[derive(Clone)]
pub struct ForgeRegistry {
    forges: Vec<Arc<dyn Forge>>,
}

impl ForgeRegistry {
    /// The built-in adapters: GitHub, then the generic git fallback.
    pub fn builtin() -> Self {
        Self {
            forges: vec![
                Arc::new(github::GitHubForge),
                Arc::new(git_generic::GitForge),
            ],
        }
    }

    /// Register `forge` ahead of the built-ins — the generic adapter
    /// claims every host, so later entries would never see one.
    #[must_use]
    pub fn register(mut self, forge: Arc<dyn Forge>) -> Self {
        self.forges.insert(0, forge);
        self
    }

    /// The adapter registered under `kind`, if any.
    pub fn by_kind(&self, kind: &str) -> Option<&dyn Forge> {
        self.forges
            .iter()
            .find(|forge| forge.kind() == kind)
            .map(Arc::as_ref)
    }

    /// The adapter for a claim's forge kind. Total: kinds without an
    /// adapter fall back to the generic git one — fetch and poll are
    /// plain git either way, and discovery checks capability itself.
    pub fn for_kind(&self, kind: &str) -> &dyn Forge {
        static GENERIC: git_generic::GitForge = git_generic::GitForge;
        self.by_kind(kind).unwrap_or(&GENERIC)
    }

    /// The adapter claiming repository URLs on `host`. Total: the
    /// generic git adapter claims everything.
    pub fn for_host(&self, host: &str) -> &dyn Forge {
        self.forges
            .iter()
            .find(|forge| forge.claims_host(host))
            .map(Arc::as_ref)
            .expect("the generic git adapter claims every host")
    }
}

/// The process-wide built-in registry, for callers without a worker's
/// injected one (URL parsing, registration defaults).
pub fn builtin() -> &'static ForgeRegistry {
    static BUILTIN: std::sync::LazyLock<ForgeRegistry> =
        std::sync::LazyLock::new(ForgeRegistry::builtin);
    &BUILTIN
}

/// The rate-limit phrasings git relays across forges — a 429, a
/// secondary-rate-limit notice, an abuse-detection trip — matched
/// case-insensitively as prose. Shared by the built-in adapters.
///
/// The needles are deliberately multi-word or punctuated phrases, never
/// bare `429`/`abuse`: the message this is fed includes the clone URL
/// (the `polling {clone_url} …` context plus git's own output), so a repo
/// slug like `acme/abuse-tracker` or `org/sloc-429` must not be mistaken
/// for the forge rate-limiting us — that would cool the whole forge down.
/// URL path segments can't contain spaces, so spaced phrases are safe.
pub(crate) fn common_rate_limit_phrasing(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "too many requests",
        "rate limit",
        "abuse detection",
        "error: 429",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_errors_are_recognized_across_a_forges_phrasings() {
        let github = github::GitHubForge;
        for message in [
            "fatal: unable to access: The requested URL returned error: 429",
            "You have exceeded a secondary rate limit",
            "remote: Too Many Requests",
            "error: RPC failed; abuse detection mechanism triggered",
        ] {
            assert!(github.is_rate_limit(message), "must flag: {message:?}");
        }
        for message in [
            "fatal: repository not found",
            "fatal: could not read Username",
            "error: unable to resolve host",
            // The message includes the clone URL (the `polling {url}`
            // context + git's output), so an ordinary failure on a repo
            // whose slug merely contains "abuse" or "429" must NOT be
            // mistaken for the forge rate-limiting us.
            "polling https://github.com/acme/abuse-tracker for its head: \
             fatal: unable to access: The requested URL returned error: 404",
            "polling https://github.com/org/sloc-429-counter for its head: \
             fatal: repository not found",
        ] {
            assert!(
                !github.is_rate_limit(message),
                "must not flag an ordinary failure: {message:?}"
            );
        }
    }

    /// A forge is added with one trait implementation plus registration:
    /// the registry resolves it by kind and by host, and its discovery
    /// capability is visible — no other code changes.
    #[test]
    fn a_test_double_forge_needs_only_an_implementation_and_registration() {
        struct FakeForge;

        impl Forge for FakeForge {
            fn kind(&self) -> &'static str {
                "fakeforge"
            }
            fn claims_host(&self, host: &str) -> bool {
                host == "fake.example"
            }
            fn default_token_env(&self) -> Option<&'static str> {
                Some("FAKE_TOKEN")
            }
            fn default_api_root(&self, base_url: &str) -> Option<String> {
                Some(format!("{base_url}/api"))
            }
            fn git_auth(&self, token: String) -> GitAuth {
                GitAuth {
                    username: "fake-user",
                    token,
                }
            }
            fn is_rate_limit(&self, message: &str) -> bool {
                message.contains("fake-limit")
            }
            fn discovery(&self) -> Option<&dyn OrgDiscovery> {
                Some(self)
            }
        }

        impl OrgDiscovery for FakeForge {
            fn list_org_repos<'a>(
                &'a self,
                _client: &'a reqwest::Client,
                _api_root: &'a str,
                org: &'a str,
                _token: Option<&'a str>,
            ) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>> {
                let org = org.to_string();
                Box::pin(async move {
                    Ok(vec![ListedRepo {
                        slug: format!("{org}/listed"),
                        visibility: yg_control::RepoVisibility::Public,
                    }])
                })
            }
        }

        let registry = ForgeRegistry::builtin().register(Arc::new(FakeForge));

        let fake = registry.by_kind("fakeforge").expect("registered by kind");
        assert_eq!(fake.default_token_env(), Some("FAKE_TOKEN"));
        assert!(fake.is_rate_limit("fake-limit"));
        assert_eq!(registry.for_host("fake.example").kind(), "fakeforge");
        assert_eq!(
            registry.for_host("github.com").kind(),
            "github",
            "built-ins keep their hosts"
        );

        let discovery = fake.discovery().expect("the double lists org repos");
        let client = reqwest::Client::new();
        let listed = futures_executor_block_on(discovery.list_org_repos(
            &client,
            "http://unused.example/api",
            "acme",
            None,
        ))
        .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].slug, "acme/listed");
    }

    /// Minimal block_on for one already-ready future — the double's
    /// listing never awaits IO, so no runtime is needed.
    fn futures_executor_block_on<T>(future: BoxFuture<'_, T>) -> T {
        let mut future = future;
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("the double's listing must be immediately ready"),
        }
    }

    #[test]
    fn unknown_kinds_resolve_to_no_adapter_and_hosts_fall_back_to_generic_git() {
        let registry = ForgeRegistry::builtin();
        assert!(registry.by_kind("gitlab").is_none());
        assert_eq!(registry.for_host("gitlab.example").kind(), "git");
        assert!(
            registry.for_host("gitlab.example").discovery().is_none(),
            "the generic adapter has no org discovery"
        );
    }
}
