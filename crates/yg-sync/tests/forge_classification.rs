use std::sync::Arc;

use yg_sync::RepoLocator;
use yg_sync::forge::{Forge, ForgeRegistry, GitAuth, OrgDiscovery};

struct CustomForge;

impl Forge for CustomForge {
    fn kind(&self) -> &'static str {
        "custom"
    }

    fn claims_host(&self, host: &str) -> bool {
        host == "forge.example"
    }

    fn default_token_env(&self) -> Option<&'static str> {
        None
    }

    fn default_api_root(&self, _base_url: &str) -> Option<String> {
        None
    }

    fn git_auth(&self, token: String) -> GitAuth {
        GitAuth {
            username: "custom-token",
            token,
        }
    }

    fn is_rate_limit(&self, _message: &str) -> bool {
        false
    }

    fn discovery(&self) -> Option<&dyn OrgDiscovery> {
        None
    }
}

#[test]
fn injected_registry_adapter_participates_in_repo_classification() {
    let registry = ForgeRegistry::builtin().register(Arc::new(CustomForge));

    let locator = RepoLocator::parse_with_registry("https://forge.example/acme/widgets", &registry)
        .expect("custom Forge URL should parse");

    assert_eq!(locator.kind, "custom");
    assert_eq!(locator.base_url, "https://forge.example");
    assert_eq!(locator.slug, "acme/widgets");
}

#[test]
fn registry_classification_preserves_builtin_and_generic_fallbacks() {
    let registry = ForgeRegistry::builtin();

    assert_eq!(
        RepoLocator::parse_with_registry("https://github.com/acme/widgets", &registry)
            .expect("GitHub URL should parse")
            .kind,
        "github"
    );
    assert_eq!(
        RepoLocator::parse_with_registry("https://git.example/acme/widgets", &registry)
            .expect("generic Git URL should parse")
            .kind,
        "git"
    );
}

#[test]
fn http_spelling_uses_https_only_for_configured_forge_lookup() {
    let unclassified =
        RepoLocator::parse_unclassified("http://forge.example/acme/widgets").unwrap();
    let lookup_base = unclassified.configured_lookup_base_url().unwrap();

    assert_eq!(lookup_base.as_str(), "https://forge.example");

    let configured = unclassified
        .clone()
        .resolve_configured(&CustomForge, &lookup_base)
        .expect("configured adapter should resolve the canonical spelling");
    assert_eq!(configured.kind, "custom");
    assert_eq!(configured.base_url, "https://forge.example");

    let locator = unclassified
        .resolve_with_registry(&ForgeRegistry::builtin())
        .expect("unknown HTTP Git URL should still resolve");
    assert_eq!(locator.kind, "git");
    assert_eq!(locator.base_url, "http://forge.example");
}
