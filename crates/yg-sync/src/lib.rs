//! Sync workers — fetch, poll, and forge-org discovery — dispatching
//! all forge-specific behavior through the [`forge::Forge`] trait.
//! GitHub is the first adapter; a generic git adapter covers plain
//! remotes.

pub mod forge;
mod git;
mod lease;
mod locator;
mod rate;
mod shutdown;
mod worker;

pub use git::{GitFetcher, MirrorGuard, forge_token, lock_mirror, mirror_path, remote_head_commit};
pub use lease::{LeaseShutdown, with_lease_heartbeat, with_lease_heartbeat_until_shutdown};
pub use locator::{LocatorError, RepoLocator, join_clone_url};
pub use shutdown::{Shutdown, ShutdownTrigger, shutdown_channel};
pub use worker::{DiscoveryConfig, PollConfig, SyncWorker};
