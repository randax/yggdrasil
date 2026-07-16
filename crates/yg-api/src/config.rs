//! Boot configuration and the running-server handle.

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use anyhow::Context;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// Server config embeds the object-store half owned by yg-shard; clients
// of this crate keep addressing it as `yg_api::ObjectStoreConfig`.
pub use yg_shard::{CacheCapacity, ObjectStoreConfig, probe_object_store};

pub const DEFAULT_TOKEN_RATE_LIMIT_REQUESTS: u32 = 120;
pub const DEFAULT_TOKEN_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
pub const DEFAULT_SEARCH_CONCURRENCY_LIMIT: usize = 8;
pub const DEFAULT_MCP_BATCH_SIZE_LIMIT: usize = 32;
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-credential request quota enforced at the authentication seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenRateLimitConfig {
    pub requests: NonZeroU32,
    pub window: Duration,
}

impl Default for TokenRateLimitConfig {
    fn default() -> Self {
        Self {
            requests: NonZeroU32::new(DEFAULT_TOKEN_RATE_LIMIT_REQUESTS)
                .expect("default token rate limit is nonzero"),
            window: DEFAULT_TOKEN_RATE_LIMIT_WINDOW,
        }
    }
}

/// Bounds applied to authenticated API work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectionConfig {
    pub token_rate_limit: TokenRateLimitConfig,
    pub search_concurrency_limit: NonZeroUsize,
    pub mcp_batch_size_limit: NonZeroUsize,
    pub request_timeout: Duration,
}

impl Default for ProtectionConfig {
    fn default() -> Self {
        Self {
            token_rate_limit: TokenRateLimitConfig::default(),
            search_concurrency_limit: NonZeroUsize::new(DEFAULT_SEARCH_CONCURRENCY_LIMIT)
                .expect("default search concurrency limit is nonzero"),
            mcp_batch_size_limit: NonZeroUsize::new(DEFAULT_MCP_BATCH_SIZE_LIMIT)
                .expect("default MCP batch size limit is nonzero"),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

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
