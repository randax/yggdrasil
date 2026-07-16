//! Boot configuration and the running-server handle.

use std::net::SocketAddr;

use anyhow::Context;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// Server config embeds the object-store half owned by yg-shard; clients
// of this crate keep addressing it as `yg_api::ObjectStoreConfig`.
pub use yg_shard::{CacheCapacity, ObjectStoreConfig, probe_object_store};

pub struct ServerConfig {
    pub listen: SocketAddr,
    pub database_url: String,
    pub object_store: ObjectStoreConfig,
    pub bootstrap_token: String,
    /// Local tier for Shard segments (RFC 0001 §6): warm Verb queries
    /// read from here instead of object storage.
    pub shard_cache: std::path::PathBuf,
    /// Maximum bytes retained in the local Shard segment tier.
    pub shard_cache_capacity: CacheCapacity,
}

/// A booted Index Server with an explicit graceful-drain trigger.
pub struct RunningServer {
    pub(crate) local_addr: SocketAddr,
    pub(crate) handle: JoinHandle<std::io::Result<()>>,
    pub(crate) shutdown: Option<oneshot::Sender<ServerShutdown>>,
}

/// Typed control message that asks axum to stop accepting and drain.
pub(crate) struct ServerShutdown;

impl RunningServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Ask axum to stop accepting new connections and drain in-flight
    /// requests. Repeated calls are harmless.
    pub fn begin_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(ServerShutdown);
        }
    }

    /// Run until the server task ends (it normally never does); a serve
    /// error surfaces here instead of being silently logged.
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        (&mut self.handle)
            .await
            .context("server task panicked")?
            .context("server exited with an error")
    }
}
