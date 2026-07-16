use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::metrics::Artifact;

/// Maximum on-disk size of the local Shard cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheCapacity(u64);

impl CacheCapacity {
    /// Ten GiB keeps the default useful on a single-node development or
    /// production deployment while preventing silent, unlimited growth.
    pub const DEFAULT: Self = Self(10 * 1024 * 1024 * 1024);

    /// Build a non-zero byte capacity.
    pub const fn new(bytes: u64) -> Option<Self> {
        if bytes == 0 { None } else { Some(Self(bytes)) }
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }

    pub(crate) fn manifest_entries(self) -> usize {
        const BYTES_PER_ENTRY: u64 = 1024 * 1024;
        const MAX_ENTRIES: u64 = 4096;
        self.0.div_ceil(BYTES_PER_ENTRY).clamp(1, MAX_ENTRIES) as usize
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct CacheKey {
    pub(crate) sha: String,
    pub(crate) artifact: Artifact,
}

#[derive(Debug)]
struct Entry {
    paths: Vec<PathBuf>,
    bytes: u64,
    used: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Evicted {
    pub(crate) key: CacheKey,
    pub(crate) paths: Vec<PathBuf>,
    bytes: u64,
    used: u64,
    displaced_cached_entry: bool,
}

impl Evicted {
    pub(crate) fn new(key: CacheKey, paths: Vec<PathBuf>, bytes: u64) -> Self {
        Self {
            key,
            paths,
            bytes,
            used: 0,
            displaced_cached_entry: false,
        }
    }

    pub(crate) fn displaced_cached_entry(&self) -> bool {
        self.displaced_cached_entry
    }
}

pub(crate) struct DiskLru {
    capacity: CacheCapacity,
    entries: HashMap<CacheKey, Entry>,
    bytes: u128,
    clock: u64,
}

impl DiskLru {
    pub(crate) fn scan(
        dir: &Path,
        capacity: CacheCapacity,
    ) -> (Self, Vec<Evicted>, Vec<(PathBuf, Artifact)>) {
        let mut lru = Self {
            capacity,
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
        };
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return (lru, Vec::new(), Vec::new());
        };
        let mut paths: Vec<PathBuf> = read_dir.flatten().map(|entry| entry.path()).collect();
        paths.sort_by_key(|path| {
            (
                std::fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::UNIX_EPOCH),
                path.clone(),
            )
        });
        let mut stale = Vec::new();
        for path in paths {
            if let Some(artifact) = stale_artifact(&path) {
                stale.push((path, artifact));
                continue;
            }
            let Some((sha, artifact)) = cache_path_key(&path) else {
                continue;
            };
            let Ok(bytes) = path_bytes(&path) else {
                continue;
            };
            let key = CacheKey { sha, artifact };
            lru.clock = lru.clock.saturating_add(1);
            let entry = lru.entries.entry(key).or_insert_with(|| Entry {
                paths: Vec::new(),
                bytes: 0,
                used: lru.clock,
            });
            entry.used = lru.clock;
            entry.paths.push(path);
            entry.bytes = entry.bytes.saturating_add(bytes);
            lru.bytes = lru.bytes.saturating_add(u128::from(bytes));
        }
        let evicted = lru.enforce(None, &HashSet::new());
        (lru, evicted, stale)
    }

    pub(crate) fn record(
        &mut self,
        key: CacheKey,
        paths: Vec<PathBuf>,
        bytes: u64,
        pinned: &HashSet<CacheKey>,
    ) -> Vec<Evicted> {
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(u128::from(old.bytes));
        }
        self.clock = self.clock.saturating_add(1);
        self.bytes = self.bytes.saturating_add(u128::from(bytes));
        self.entries.insert(
            key.clone(),
            Entry {
                paths,
                bytes,
                used: self.clock,
            },
        );
        self.enforce(Some(&key), pinned)
    }

    pub(crate) fn touch(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.get_mut(key) {
            self.clock = self.clock.saturating_add(1);
            entry.used = self.clock;
        }
    }

    pub(crate) fn restore(&mut self, eviction: Evicted) {
        if let Some(old) = self.entries.remove(&eviction.key) {
            self.bytes = self.bytes.saturating_sub(u128::from(old.bytes));
        }
        self.bytes = self.bytes.saturating_add(u128::from(eviction.bytes));
        self.entries.insert(
            eviction.key,
            Entry {
                paths: eviction.paths,
                bytes: eviction.bytes,
                used: eviction.used,
            },
        );
    }

    pub(crate) fn restore_existing(&mut self, mut eviction: Evicted) {
        let fallback = eviction.clone();
        eviction.paths.retain(|path| path.exists());
        let bytes = match measure_paths(&eviction.paths) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.restore(fallback);
                return;
            }
        };
        if bytes == 0 {
            return;
        }
        eviction.bytes = bytes;
        self.restore(eviction);
    }

    pub(crate) fn restore_failed_eviction(&mut self, mut eviction: Evicted) {
        let mut stale = Vec::new();
        eviction.paths.retain(|path| {
            if let Some(artifact) = stale_artifact(path) {
                stale.push((path.clone(), artifact));
                false
            } else {
                true
            }
        });
        self.restore_existing(eviction);
        for (path, artifact) in stale {
            let bytes = measure_paths(std::slice::from_ref(&path)).unwrap_or(u64::MAX);
            self.track_stale(path, bytes, artifact);
        }
    }

    pub(crate) fn take(&mut self, key: &CacheKey) -> Option<Evicted> {
        let entry = self.entries.remove(key)?;
        self.bytes = self.bytes.saturating_sub(u128::from(entry.bytes));
        Some(Evicted {
            key: key.clone(),
            paths: entry.paths,
            bytes: entry.bytes,
            used: entry.used,
            displaced_cached_entry: false,
        })
    }

    pub(crate) fn track(&mut self, key: CacheKey, paths: Vec<PathBuf>, bytes: u64) {
        self.clock = self.clock.saturating_add(1);
        if let Some(old) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(u128::from(old.bytes));
        }
        self.bytes = self.bytes.saturating_add(u128::from(bytes));
        self.entries.insert(
            key,
            Entry {
                paths,
                bytes,
                used: self.clock,
            },
        );
    }

    pub(crate) fn track_stale(&mut self, path: PathBuf, bytes: u64, artifact: Artifact) {
        let key = CacheKey {
            sha: format!("stale:{}", path.display()),
            artifact,
        };
        self.track(key, vec![path], bytes);
    }

    fn enforce(
        &mut self,
        protected: Option<&CacheKey>,
        pinned: &HashSet<CacheKey>,
    ) -> Vec<Evicted> {
        let mut evicted = Vec::new();
        while self.bytes > u128::from(self.capacity.bytes()) {
            let candidate = self
                .entries
                .iter()
                .filter(|(key, _)| protected != Some(*key) && !pinned.contains(*key))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
                .or_else(|| protected.cloned());
            let Some(key) = candidate else { break };
            let Some(entry) = self.entries.remove(&key) else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(u128::from(entry.bytes));
            evicted.push(Evicted {
                displaced_cached_entry: protected != Some(&key),
                key,
                paths: entry.paths,
                bytes: entry.bytes,
                used: entry.used,
            });
        }
        evicted
    }
}

fn cache_path_key(path: &Path) -> Option<(String, Artifact)> {
    let name = path.file_name()?.to_str()?;
    let (sha, artifact) = if let Some(sha) = name.strip_suffix(".sqlite") {
        (sha, Artifact::Graph)
    } else if let Some(sha) = name.strip_suffix(".tar") {
        (sha, Artifact::Fts)
    } else if let Some(sha) = name.strip_suffix(".fts") {
        (sha, Artifact::Fts)
    } else {
        return None;
    };
    (sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| (sha.to_string(), artifact))
}

fn stale_artifact(path: &Path) -> Option<Artifact> {
    let name = path.file_name()?.to_str()?;
    if name.contains(".sqlite.tmp-") {
        Some(Artifact::Graph)
    } else if name.contains(".tar.tmp-") || name.contains(".fts.tmp-") {
        Some(Artifact::Fts)
    } else {
        None
    }
}

fn path_bytes(path: &Path) -> std::io::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Shard-cache entries cannot be symbolic links",
        ));
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(path)? {
        bytes = bytes.saturating_add(path_bytes(&entry?.path())?);
    }
    Ok(bytes)
}

#[derive(Debug)]
pub(crate) struct RemoveCachedPathError {
    source: std::io::Error,
    remaining_path: PathBuf,
}

impl RemoveCachedPathError {
    pub(crate) fn remaining_path(&self) -> &Path {
        &self.remaining_path
    }
}

impl std::fmt::Display for RemoveCachedPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for RemoveCachedPathError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn remove_cached_path(path: &Path) -> Result<(), RemoveCachedPathError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(RemoveCachedPathError {
                source,
                remaining_path: path.to_path_buf(),
            });
        }
    };
    if !metadata.is_dir() {
        return std::fs::remove_file(path).map_err(|source| RemoveCachedPathError {
            source,
            remaining_path: path.to_path_buf(),
        });
    }
    if path.extension().is_some_and(|extension| extension == "fts") {
        let staged = eviction_staging_path(path);
        match std::fs::rename(path, &staged) {
            Ok(()) => {
                return std::fs::remove_dir_all(&staged).map_err(|source| RemoveCachedPathError {
                    source,
                    remaining_path: staged,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(RemoveCachedPathError {
                    source,
                    remaining_path: path.to_path_buf(),
                });
            }
        }
    }
    std::fs::remove_dir_all(path).map_err(|source| RemoveCachedPathError {
        source,
        remaining_path: path.to_path_buf(),
    })
}

fn eviction_staging_path(path: &Path) -> PathBuf {
    static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(0);

    let file_name = path
        .file_name()
        .expect("a cached path has a file name")
        .to_string_lossy();
    loop {
        let id = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = path.with_file_name(format!("{file_name}.tmp-{}-{id}", std::process::id()));
        if !candidate.exists() {
            return candidate;
        }
    }
}

pub(crate) struct CleanupPath(Option<PathBuf>);

impl CleanupPath {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

#[derive(Default)]
struct PinState {
    counts: HashMap<CacheKey, usize>,
    evicting: HashSet<CacheKey>,
    lease_generation: u64,
    eviction_generation: u64,
}

#[derive(Default)]
pub(crate) struct Pins {
    state: Mutex<PinState>,
    lease_released: tokio::sync::Notify,
    eviction_finished: tokio::sync::Notify,
}

impl Pins {
    pub(crate) fn try_pin(self: &Arc<Self>, key: CacheKey) -> Result<CacheLease, u64> {
        let mut state = self.state.lock().expect("Shard-cache pin lock poisoned");
        if state.evicting.contains(&key) {
            return Err(state.eviction_generation);
        }
        *state.counts.entry(key.clone()).or_default() += 1;
        Ok(CacheLease {
            pins: self.clone(),
            key,
        })
    }

    pub(crate) fn pinned(&self) -> (HashSet<CacheKey>, u64) {
        let state = self.state.lock().expect("Shard-cache pin lock poisoned");
        (
            state.counts.keys().cloned().collect(),
            state.lease_generation,
        )
    }

    pub(crate) fn begin_eviction(&self, key: &CacheKey) -> bool {
        let mut state = self.state.lock().expect("Shard-cache pin lock poisoned");
        if state.counts.contains_key(key) {
            return false;
        }
        state.evicting.insert(key.clone());
        true
    }

    pub(crate) fn finish_eviction(&self, key: &CacheKey) {
        let mut state = self.state.lock().expect("Shard-cache pin lock poisoned");
        state.evicting.remove(key);
        state.eviction_generation = state.eviction_generation.wrapping_add(1);
        drop(state);
        self.eviction_finished.notify_waiters();
    }

    pub(crate) async fn wait_for_lease_release(&self, observed_generation: u64) {
        loop {
            let notified = self.lease_released.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .state
                .lock()
                .expect("Shard-cache pin lock poisoned")
                .lease_generation
                != observed_generation
            {
                return;
            }
            notified.await;
        }
    }

    pub(crate) async fn wait_for_eviction(&self, observed_generation: u64) {
        loop {
            let notified = self.eviction_finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .state
                .lock()
                .expect("Shard-cache pin lock poisoned")
                .eviction_generation
                != observed_generation
            {
                return;
            }
            notified.await;
        }
    }
}

pub struct CacheLease {
    pins: Arc<Pins>,
    key: CacheKey,
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        let mut state = self
            .pins
            .state
            .lock()
            .expect("Shard-cache pin lock poisoned");
        let mut became_evictable = false;
        if let Some(count) = state.counts.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                state.counts.remove(&self.key);
                became_evictable = true;
            }
        }
        if became_evictable {
            state.lease_generation = state.lease_generation.wrapping_add(1);
        }
        drop(state);
        if became_evictable {
            self.pins.lease_released.notify_waiters();
        }
    }
}

pub struct LeasedPath {
    pub path: PathBuf,
    pub lease: CacheLease,
}

impl Drop for CleanupPath {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = remove_cached_path(&path);
        }
    }
}

pub(crate) fn measure_paths(paths: &[PathBuf]) -> std::io::Result<u64> {
    let mut bytes = 0_u64;
    for path in paths {
        bytes = bytes.saturating_add(path_bytes(path)?);
    }
    Ok(bytes)
}

pub(crate) struct MemoryLru<K, V> {
    entries: HashMap<K, (V, u64)>,
    capacity: usize,
    clock: u64,
}

impl<K: Clone + Eq + std::hash::Hash, V: Clone> MemoryLru<K, V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            clock: 0,
        }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        let (value, used) = self.entries.get_mut(key)?;
        self.clock = self.clock.saturating_add(1);
        *used = self.clock;
        Some(value.clone())
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        self.clock = self.clock.saturating_add(1);
        self.entries.insert(key, (value, self.clock));
        while self.entries.len() > self.capacity {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
    }

    pub(crate) fn remove(&mut self, key: &K) {
        self.entries.remove(key);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.entries.retain(|key, (value, _)| keep(key, value));
    }
}

#[derive(Default)]
pub(crate) struct Flights {
    gates: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl Flights {
    pub(crate) fn gate(&self, checksum: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.gates.lock().expect("shard cache flight lock poisoned");
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(checksum).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(checksum.to_string(), Arc::downgrade(&gate));
        gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_lru_evicts_the_least_recently_used_entry() {
        let mut lru = MemoryLru::new(2);
        lru.insert("a", 1);
        lru.insert("b", 2);
        assert_eq!(lru.get(&"a"), Some(1));
        lru.insert("c", 3);
        assert_eq!(lru.get(&"b"), None);
        assert_eq!(lru.get(&"a"), Some(1));
        assert_eq!(lru.get(&"c"), Some(3));
    }

    #[test]
    fn startup_scan_accounts_only_content_addressed_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let old_sha = "1".repeat(64);
        let new_sha = "2".repeat(64);
        std::fs::write(dir.path().join(format!("{old_sha}.sqlite")), vec![0; 8]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(dir.path().join(format!("{new_sha}.sqlite")), vec![0; 8]).unwrap();
        let stale_path = dir.path().join(format!("{old_sha}.sqlite.tmp-1-1"));
        std::fs::write(&stale_path, vec![0; 100]).unwrap();
        let stale_fts_path = dir.path().join(format!("{new_sha}.fts.tmp-1-2"));
        std::fs::create_dir(&stale_fts_path).unwrap();
        std::fs::write(stale_fts_path.join("partial-index"), vec![0; 100]).unwrap();

        let (lru, evicted, stale) = DiskLru::scan(dir.path(), CacheCapacity::new(8).unwrap());

        assert_eq!(lru.bytes, 8);
        assert_eq!(evicted.len(), 1);
        assert_eq!(
            stale,
            [
                (stale_path, Artifact::Graph),
                (stale_fts_path, Artifact::Fts)
            ]
        );
        assert_eq!(evicted[0].key.sha, old_sha);
        assert!(evicted[0].paths[0].ends_with(format!("{old_sha}.sqlite")));
    }

    #[test]
    fn fts_eviction_renames_before_removing_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sha = "3".repeat(64);
        let unpacked = dir.path().join(format!("{sha}.fts"));
        std::fs::create_dir(&unpacked).unwrap();
        std::fs::write(unpacked.join("index"), b"contents").unwrap();

        remove_cached_path(&unpacked).unwrap();

        assert!(!unpacked.exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn failed_staged_eviction_stays_accounted_across_same_sha_recording() {
        let dir = tempfile::tempdir().unwrap();
        let sha = "4".repeat(64);
        let staged = dir.path().join(format!("{sha}.fts.tmp-1-1"));
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("partial-index"), vec![0; 8]).unwrap();
        let key = CacheKey {
            sha: sha.clone(),
            artifact: Artifact::Fts,
        };
        let mut lru = DiskLru {
            capacity: CacheCapacity::new(64).unwrap(),
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
        };
        lru.restore_failed_eviction(Evicted {
            key: key.clone(),
            paths: vec![staged.clone()],
            bytes: 8,
            used: 1,
            displaced_cached_entry: true,
        });

        let replacement = dir.path().join(format!("{sha}.tar"));
        std::fs::write(&replacement, vec![0; 1]).unwrap();
        lru.record(key.clone(), vec![replacement], 1, &HashSet::new());

        assert!(lru.entries.contains_key(&key));
        assert!(
            lru.entries
                .values()
                .any(|entry| entry.paths.iter().any(|path| path == &staged))
        );
        assert_eq!(lru.bytes, 9);
    }
}
