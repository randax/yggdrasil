//! The generic git adapter: any plain remote without a forge API —
//! file:// fixtures, self-hosted mirrors. Claims every host, so it
//! registers last and terminates host lookup.

use super::{Forge, GitAuth, OrgDiscovery, common_rate_limit_phrasing};

pub(crate) struct GitForge;

impl Forge for GitForge {
    fn kind(&self) -> &'static str {
        "git"
    }

    /// Every host: the fallback for remotes no other adapter claims.
    fn claims_host(&self, _host: &str) -> bool {
        true
    }

    fn default_token_env(&self) -> Option<&'static str> {
        None
    }

    /// Plain remotes have no REST API.
    fn default_api_root(&self, _base_url: &str) -> Option<String> {
        None
    }

    /// The conventional token username most git hosts accept; a forge
    /// that requires its own gets an adapter.
    fn git_auth(&self, token: String) -> GitAuth {
        GitAuth {
            username: "x-access-token",
            token,
        }
    }

    fn is_rate_limit(&self, message: &str) -> bool {
        common_rate_limit_phrasing(message)
    }

    fn discovery(&self) -> Option<&dyn OrgDiscovery> {
        None
    }
}
