//! Reading a cached artifact while it is pinned.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::task::JoinError;

use crate::{CacheLease, FtsIndex, LeasedPath, open_fts};

/// A local artifact, pinned against eviction until it is read. Readers borrow
/// the open handle, so it cannot outlive the pin that protects its files.
pub struct PinnedArtifact {
    path: PathBuf,
    _lease: Option<CacheLease>,
}

impl From<LeasedPath> for PinnedArtifact {
    fn from(leased: LeasedPath) -> Self {
        Self {
            path: leased.path,
            _lease: Some(leased.lease),
        }
    }
}

impl PinnedArtifact {
    /// An artifact outside any cache, so nothing can evict it.
    pub fn unpinned(path: PathBuf) -> Self {
        Self { path, _lease: None }
    }

    /// Open the graph segment and run `read` off the runtime threads. The
    /// outer error is a panicked read; the inner one is a failed open or read.
    pub async fn read_graph<T, F>(self, read: F) -> Result<anyhow::Result<T>, JoinError>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    {
        self.read(open_graph, read).await
    }

    /// Open the FTS segment and run `read` off the runtime threads, with the
    /// same outcomes as [`Self::read_graph`].
    pub async fn read_fts<T, F>(self, read: F) -> Result<anyhow::Result<T>, JoinError>
    where
        T: Send + 'static,
        F: FnOnce(&FtsIndex) -> anyhow::Result<T> + Send + 'static,
    {
        self.read(open_fts, read).await
    }

    async fn read<H, T, F>(
        self,
        open: fn(&Path) -> anyhow::Result<H>,
        read: F,
    ) -> Result<anyhow::Result<T>, JoinError>
    where
        H: 'static,
        T: Send + 'static,
        F: FnOnce(&H) -> anyhow::Result<T> + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            // Eviction is pin-gated, and locals drop in reverse order, even on
            // unwind, so the handle closes before the artifact is evictable.
            // Binding all of `self` here also keeps the lease in this closure.
            let pinned = self;
            let handle = open(&pinned.path)?;
            read(&handle)
        })
        .await
    }
}

fn open_graph(path: &Path) -> anyhow::Result<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening the cached graph segment")
}
