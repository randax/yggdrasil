//! Forge trait + GitHub/GitLab/Forgejo (Codeberg runs Forgejo) adapters, webhooks.

mod forge;
mod git;
mod lease;
mod locator;
mod rate;
mod worker;

pub use git::{GitFetcher, MirrorGuard, forge_token, lock_mirror, mirror_path, remote_head_commit};
pub use lease::with_lease_heartbeat;
pub use locator::{ForgeKind, RepoLocator, join_clone_url};
pub use worker::{DiscoveryConfig, PollConfig, SyncWorker};
