//! Sync workers — fetch, poll, and forge-org discovery — dispatching
//! all forge-specific behavior through the [`forge::Forge`] trait.
//! GitHub is the first adapter; a generic git adapter covers plain
//! remotes.

pub mod forge;
mod git;
mod lease;
mod locator;
mod rate;
mod worker;

pub use git::{GitFetcher, MirrorGuard, forge_token, lock_mirror, mirror_path, remote_head_commit};
pub use lease::with_lease_heartbeat;
pub use locator::{RepoLocator, join_clone_url};
pub use worker::{DiscoveryConfig, PollConfig, SyncWorker};
