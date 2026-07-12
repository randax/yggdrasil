//! Boot configuration and the running-server handle.

use std::net::SocketAddr;

use anyhow::Context;
use tokio::task::JoinHandle;

// Server config embeds the object-store half owned by yg-shard; clients
// of this crate keep addressing it as `yg_api::ObjectStoreConfig`.
pub use yg_shard::{ObjectStoreConfig, probe_object_store};

pub struct ServerConfig {
    pub listen: SocketAddr,
    pub database_url: String,
    pub object_store: ObjectStoreConfig,
    pub bootstrap_token: String,
    /// Local tier for Shard segments (RFC 0001 §6): warm Verb queries
    /// read from here instead of object storage.
    pub shard_cache: std::path::PathBuf,
}

/// A booted Index Server, listening until dropped or the process exits.
pub struct RunningServer {
    pub(crate) local_addr: SocketAddr,
    pub(crate) handle: JoinHandle<std::io::Result<()>>,
}

impl RunningServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run until the server task ends (it normally never does); a serve
    /// error surfaces here instead of being silently logged.
    pub async fn wait(self) -> anyhow::Result<()> {
        self.handle
            .await
            .context("server task panicked")?
            .context("server exited with an error")
    }
}
