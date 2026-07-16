//! Bounded-cache contract seams kept separate from the original cache suite.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::{StreamExt, stream};
use object_store::memory::InMemory;
use object_store::{
    GetOptions, GetResult, GetResultPayload, ObjectStore, ObjectStoreExt, PutOptions, PutPayload,
    PutResult,
};
use tokio::sync::Notify;
use yg_shard::{
    CacheArtifactTooLarge, CacheCapacity, CacheCapacityUnavailable, Edge, Graph, Metrics, Node,
    SearchDoc, ShardCache, fts_segment_key, graph_segment_key, write_shard,
};

const STREAM_CHUNK_BYTES: usize = 4 * 1024;

#[derive(Debug)]
struct ObservedStore {
    inner: InMemory,
    targets: std::sync::Mutex<HashSet<String>>,
    target_gets: AtomicUsize,
    hold_first_target_get: AtomicBool,
    first_target_get_started: Notify,
    release_first_target_get: Notify,
    stream_target: AtomicBool,
    cache_dir: std::sync::Mutex<Option<std::path::PathBuf>>,
    peak_unstaged_bytes: Arc<AtomicUsize>,
}

impl ObservedStore {
    fn new() -> Self {
        Self {
            inner: InMemory::new(),
            targets: std::sync::Mutex::new(HashSet::new()),
            target_gets: AtomicUsize::new(0),
            hold_first_target_get: AtomicBool::new(false),
            first_target_get_started: Notify::new(),
            release_first_target_get: Notify::new(),
            stream_target: AtomicBool::new(false),
            cache_dir: std::sync::Mutex::new(None),
            peak_unstaged_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn observe_only(&self, key: String) {
        self.targets.lock().unwrap().clear();
        self.targets.lock().unwrap().insert(key);
        self.target_gets.store(0, Ordering::SeqCst);
    }

    fn observe_all(&self, keys: impl IntoIterator<Item = String>) {
        let mut targets = self.targets.lock().unwrap();
        targets.clear();
        targets.extend(keys);
        self.target_gets.store(0, Ordering::SeqCst);
    }

    fn target_gets(&self) -> usize {
        self.target_gets.load(Ordering::SeqCst)
    }

    fn hold_first_target_get(&self) {
        self.hold_first_target_get.store(true, Ordering::SeqCst);
    }

    fn stream_target_into(&self, cache_dir: &std::path::Path) {
        *self.cache_dir.lock().unwrap() = Some(cache_dir.to_path_buf());
        self.stream_target.store(true, Ordering::SeqCst);
        self.peak_unstaged_bytes.store(0, Ordering::SeqCst);
    }

    fn peak_unstaged_bytes(&self) -> usize {
        self.peak_unstaged_bytes.load(Ordering::SeqCst)
    }

    async fn object_size(&self, key: &str) -> u64 {
        self.inner
            .head(&key.into())
            .await
            .expect("published object has metadata")
            .size
    }
}

impl std::fmt::Display for ObservedStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "ObservedStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for ObservedStore {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let targeted = self.targets.lock().unwrap().contains(location.as_ref());
        if targeted {
            let attempt = self.target_gets.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 && self.hold_first_target_get.load(Ordering::SeqCst) {
                self.first_target_get_started.notify_one();
                self.release_first_target_get.notified().await;
            }
        }

        let result = self.inner.get_opts(location, options).await?;
        if !targeted || !self.stream_target.load(Ordering::SeqCst) {
            return Ok(result);
        }

        let meta = result.meta.clone();
        let range = result.range.clone();
        let attributes = result.attributes.clone();
        let bytes = result.bytes().await?;
        let chunks: Vec<_> = (0..bytes.len())
            .step_by(STREAM_CHUNK_BYTES)
            .map(|start| {
                let end = (start + STREAM_CHUNK_BYTES).min(bytes.len());
                Ok::<_, object_store::Error>(bytes.slice(start..end))
            })
            .collect();
        let yielded = Arc::new(AtomicUsize::new(0));
        let cache_dir = self.cache_dir.lock().unwrap().clone().unwrap();
        let peak = self.peak_unstaged_bytes.clone();
        let payload = stream::iter(chunks)
            .map(move |chunk| {
                let chunk = chunk?;
                let staged = staged_temp_file_bytes(&cache_dir);
                let after_yield = yielded.fetch_add(chunk.len(), Ordering::SeqCst) + chunk.len();
                peak.fetch_max(after_yield.saturating_sub(staged), Ordering::SeqCst);
                Ok(chunk)
            })
            .boxed();
        Ok(GetResult {
            payload: GetResultPayload::Stream(payload),
            meta,
            range,
            attributes,
        })
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

fn staged_temp_file_bytes(cache_dir: &std::path::Path) -> usize {
    std::fs::read_dir(cache_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| usize::try_from(metadata.len()).unwrap())
        .max()
        .unwrap_or(0)
}

async fn publish_graph(store: &dyn ObjectStore, repo_id: i64, label: &str, files: usize) -> String {
    let nodes = (0..files)
        .map(|index| {
            let path = format!("{label}/{index:04}.rs");
            Node::file(&path)
        })
        .collect();
    write_shard(
        store,
        repo_id,
        label,
        Graph {
            nodes,
            edges: Vec::<Edge>::new(),
        },
        Vec::<SearchDoc>::new(),
    )
    .await
    .expect("publish graph fixture")
    .revision
}

#[tokio::test]
async fn deleting_a_process_verified_file_causes_a_clean_refetch() {
    let store = Arc::new(ObservedStore::new());
    let revision = publish_graph(store.as_ref(), 41, "deleted", 3).await;
    let key = graph_segment_key(41, &revision);
    let directory = tempfile::tempdir().unwrap();
    let cache = ShardCache::new(store.clone(), directory.path());

    let path = cache.graph_path(41, &revision).await.unwrap();
    let pristine = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    store.observe_only(key);

    let repaired = cache.graph_path(41, &revision).await.unwrap();
    assert_eq!(repaired, path);
    assert_eq!(std::fs::read(repaired).unwrap(), pristine);
    assert_eq!(
        store.target_gets(),
        1,
        "the missing artifact is fetched once"
    );
}

#[tokio::test]
async fn concurrent_cold_reads_fetch_one_content_hash_once() {
    let store = Arc::new(ObservedStore::new());
    let revision = publish_graph(store.as_ref(), 42, "single-flight", 3).await;
    let key = graph_segment_key(42, &revision);
    let directory = tempfile::tempdir().unwrap();
    let cache = Arc::new(ShardCache::new(store.clone(), directory.path()));

    let path = cache.graph_path(42, &revision).await.unwrap();
    std::fs::remove_file(path).unwrap();
    store.observe_only(key);
    store.hold_first_target_get();

    let first_cache = cache.clone();
    let first_revision = revision.clone();
    let first = tokio::spawn(async move { first_cache.graph_path(42, &first_revision).await });
    store.first_target_get_started.notified().await;
    let second_cache = cache.clone();
    let second_revision = revision.clone();
    let second_started = Arc::new(tokio::sync::Barrier::new(2));
    let second_task_started = second_started.clone();
    let second = tokio::spawn(async move {
        second_task_started.wait().await;
        second_cache.graph_path(42, &second_revision).await
    });
    second_started.wait().await;
    // Give the second task repeated scheduling opportunities while the
    // leader is held inside object storage. Without single-flight it will
    // deterministically enter the store and increment the target count.
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    store.release_first_target_get.notify_one();

    let first_path = first.await.unwrap().unwrap();
    let second_path = second.await.unwrap().unwrap();
    assert_eq!(first_path, second_path);
    assert!(first_path.is_file());
    assert_eq!(
        store.target_gets(),
        1,
        "the content hash has one leader fetch"
    );
}

#[tokio::test]
async fn fetched_bytes_are_staged_while_the_object_stream_is_consumed() {
    let store = Arc::new(ObservedStore::new());
    let revision = publish_graph(store.as_ref(), 43, "streamed", 2_000).await;
    let key = graph_segment_key(43, &revision);
    let object_bytes = usize::try_from(store.object_size(&key).await).unwrap();
    assert!(
        object_bytes > STREAM_CHUNK_BYTES * 8,
        "fixture must have many chunks"
    );
    let directory = tempfile::tempdir().unwrap();
    store.observe_only(key);
    store.stream_target_into(directory.path());
    let cache = ShardCache::new(store.clone(), directory.path());

    let path = cache.graph_path(43, &revision).await.unwrap();

    assert!(path.is_file());
    assert_eq!(store.target_gets(), 1);
    assert!(
        store.peak_unstaged_bytes() <= STREAM_CHUNK_BYTES * 2,
        "the cache got {} bytes ahead of its staging file for a {object_bytes}-byte object",
        store.peak_unstaged_bytes()
    );
}

#[tokio::test]
async fn graph_artifacts_are_evicted_in_least_recently_used_order() {
    let store = Arc::new(ObservedStore::new());
    let revisions = [
        publish_graph(store.as_ref(), 44, "alpha", 20).await,
        publish_graph(store.as_ref(), 44, "bravo", 30).await,
        publish_graph(store.as_ref(), 44, "charlie", 40).await,
    ];
    let keys: Vec<_> = revisions
        .iter()
        .map(|revision| graph_segment_key(44, revision))
        .collect();
    let mut sizes = Vec::new();
    for key in &keys {
        sizes.push(store.object_size(key).await);
    }
    sizes.sort_unstable();
    let capacity_bytes = sizes[1] + sizes[2];
    let capacity = CacheCapacity::new(capacity_bytes).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cache = ShardCache::with_capacity(store.clone(), directory.path(), capacity);

    let alpha = cache.graph_path(44, &revisions[0]).await.unwrap();
    let bravo = cache.graph_path(44, &revisions[1]).await.unwrap();
    assert_eq!(cache.graph_path(44, &revisions[0]).await.unwrap(), alpha);
    let charlie = cache.graph_path(44, &revisions[2]).await.unwrap();

    assert!(alpha.is_file(), "the recently touched artifact remains");
    assert!(
        !bravo.exists(),
        "the least recently used artifact is evicted"
    );
    assert!(charlie.is_file(), "the newly fetched artifact remains");
    assert!(
        regular_file_bytes(directory.path()) <= capacity_bytes,
        "the cache stays within its configured byte cap"
    );

    store.observe_all(keys);
    cache.graph_path(44, &revisions[1]).await.unwrap();
    assert_eq!(
        store.target_gets(),
        1,
        "an evicted artifact is fetched once"
    );
    assert!(regular_file_bytes(directory.path()) <= capacity_bytes);
}

#[tokio::test]
async fn leased_graph_artifact_blocks_eviction_until_the_lease_drops() {
    let store = Arc::new(ObservedStore::new());
    let first_revision = publish_graph(store.as_ref(), 45, "first", 25).await;
    let second_revision = publish_graph(store.as_ref(), 45, "other", 25).await;
    let first_size = store
        .object_size(&graph_segment_key(45, &first_revision))
        .await;
    let second_size = store
        .object_size(&graph_segment_key(45, &second_revision))
        .await;
    let capacity_bytes = first_size.max(second_size);
    let directory = tempfile::tempdir().unwrap();
    let cache = ShardCache::with_capacity(
        store.clone(),
        directory.path(),
        CacheCapacity::new(capacity_bytes).unwrap(),
    );

    let leased = cache.leased_graph_path(45, &first_revision).await.unwrap();
    let duplicate_lease = cache.leased_graph_path(45, &first_revision).await.unwrap();
    let err = cache
        .graph_path(45, &second_revision)
        .await
        .expect_err("a pinned artifact must not be evicted");
    assert!(
        err.downcast_ref::<CacheCapacityUnavailable>().is_some(),
        "capacity contention remains typed: {err:#}"
    );
    assert!(leased.path.is_file(), "the leased path remains readable");
    assert!(regular_file_bytes(directory.path()) <= capacity_bytes);

    store.observe_only(graph_segment_key(45, &second_revision));
    // Rendezvous at the observed store request, rather than assuming that
    // yielding after spawning means the request has reached object storage.
    store.hold_first_target_get();
    let first_path = leased.path.clone();
    let waiting_cache = Arc::new(cache);
    let task_cache = waiting_cache.clone();
    let waiting_revision = second_revision.clone();
    let waiting =
        tokio::spawn(async move { task_cache.leased_graph_path(45, &waiting_revision).await });
    store.first_target_get_started.notified().await;
    assert!(
        !waiting.is_finished(),
        "leased resolution remains pending while its fetch is held"
    );
    assert_eq!(
        store.target_gets(),
        1,
        "the losing artifact starts one fetch"
    );

    drop(leased);
    store.release_first_target_get.notify_one();
    // Queue behind the waiter's checksum flight. This typed failure proves
    // that the held fetch completed and entered capacity waiting while the
    // duplicate lease was still pinned; no scheduler timing is assumed.
    let probe_error = waiting_cache
        .graph_path(45, &second_revision)
        .await
        .expect_err("the remaining lease still prevents replacement");
    assert!(
        probe_error
            .downcast_ref::<CacheCapacityUnavailable>()
            .is_some(),
        "the post-flight capacity probe remains typed: {probe_error:#}"
    );
    assert!(
        !waiting.is_finished(),
        "a partial reference-count decrement does not wake capacity waiters"
    );
    // The probe above establishes the capacity-waiting phase. These yields
    // are only adversarial scheduling opportunities: an implementation that
    // retries instead of parking would now add another observed fetch.
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store.target_gets(),
        2,
        "the waiter and the explicit post-flight probe each fetch once"
    );
    drop(duplicate_lease);
    let second = waiting.await.unwrap().unwrap();
    assert!(second.path.is_file());
    assert!(
        !first_path.exists(),
        "the unpinned artifact can now be evicted"
    );
    assert!(regular_file_bytes(directory.path()) <= capacity_bytes);
    assert_eq!(
        store.target_gets(),
        3,
        "the waiter retries exactly once after the final lease drops"
    );
    drop(second);
}

#[tokio::test]
async fn concurrent_distinct_cold_fetches_never_return_an_evicted_leased_path() {
    let store = Arc::new(ObservedStore::new());
    let first_revision = publish_graph(store.as_ref(), 48, "racer-a", 25).await;
    let second_revision = publish_graph(store.as_ref(), 48, "racer-b", 25).await;
    let keys = [
        graph_segment_key(48, &first_revision),
        graph_segment_key(48, &second_revision),
    ];
    let first_size = store.object_size(&keys[0]).await;
    let second_size = store.object_size(&keys[1]).await;
    let capacity_bytes = first_size.max(second_size);
    let directory = tempfile::tempdir().unwrap();
    let cache = Arc::new(ShardCache::with_capacity(
        store.clone(),
        directory.path(),
        CacheCapacity::new(capacity_bytes).unwrap(),
    ));
    store.observe_all(keys);
    // Hold the first fetch after it is counted, then let the second caller
    // obtain its lease. Without this ordering, a legal eviction between
    // graph_path returning and leased_graph_path pinning can require a third
    // recovery fetch to avoid returning the now-absent first path.
    store.hold_first_target_get();

    let first_cache = cache.clone();
    let first_task_revision = first_revision.clone();
    let first_task = tokio::spawn(async move {
        first_cache
            .leased_graph_path(48, &first_task_revision)
            .await
    });
    store.first_target_get_started.notified().await;
    let second_cache = cache.clone();
    let second_task_revision = second_revision.clone();
    let second_task = tokio::spawn(async move {
        second_cache
            .leased_graph_path(48, &second_task_revision)
            .await
    });
    let leased = second_task.await.unwrap();
    let leased = leased.expect("one overlapping leased fetch succeeds");
    assert!(leased.path.is_file());

    for _ in 0..1_000 {
        if regular_file_bytes(directory.path()) <= capacity_bytes {
            break;
        }
        tokio::task::yield_now().await;
    }
    let loser_pending = !first_task.is_finished();
    assert!(
        loser_pending,
        "the other leased fetch waits without returning a stale path"
    );
    assert_eq!(
        store.target_gets(),
        2,
        "each overlapping cold artifact is fetched once before capacity waiting"
    );
    assert!(
        regular_file_bytes(directory.path()) <= capacity_bytes,
        "the pinned winner remains within capacity while the losing fetch is held"
    );
    store.release_first_target_get.notify_one();

    // The losing checksum must not remain falsely verified: resolving it
    // while the winner is pinned cannot return a stale, absent path.
    let error = cache
        .graph_path(48, &first_revision)
        .await
        .expect_err("the other checksum cannot displace a leased winner");
    assert!(error.downcast_ref::<CacheCapacityUnavailable>().is_some());
    // The probe establishes that the original losing flight completed.
    // Give an illegal immediate retry time to reveal itself before the
    // winner's lease is released and a retry becomes legitimate.
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store.target_gets(),
        3,
        "the losing flight and the explicit post-flight probe each fetch once"
    );
    assert!(leased.path.is_file());
    assert!(regular_file_bytes(directory.path()) <= capacity_bytes);

    let first_path = leased.path.clone();
    drop(leased);
    let replacement = first_task.await.unwrap().unwrap();
    assert!(replacement.path.is_file());
    assert!(
        !first_path.exists(),
        "the released winner is evicted during the waiting handoff"
    );
    assert!(regular_file_bytes(directory.path()) <= capacity_bytes);
    assert_eq!(
        store.target_gets(),
        4,
        "the waiting caller retries exactly once after the winner lease drops"
    );
    drop(replacement);
}

#[tokio::test]
async fn capacity_eviction_is_exposed_as_its_own_metric() {
    use prometheus_client::encoding::text::encode;
    use prometheus_client::registry::Registry;

    let store = Arc::new(ObservedStore::new());
    let first_revision = publish_graph(store.as_ref(), 46, "metric-a", 25).await;
    let second_revision = publish_graph(store.as_ref(), 46, "metric-b", 25).await;
    let first_size = store
        .object_size(&graph_segment_key(46, &first_revision))
        .await;
    let second_size = store
        .object_size(&graph_segment_key(46, &second_revision))
        .await;
    let capacity = CacheCapacity::new(first_size.max(second_size)).unwrap();
    let mut registry = Registry::default();
    let metrics = Metrics::registered(&mut registry);
    let directory = tempfile::tempdir().unwrap();
    let cache = ShardCache::with_metrics_and_capacity(store, directory.path(), metrics, capacity);

    cache.graph_path(46, &first_revision).await.unwrap();
    cache.graph_path(46, &second_revision).await.unwrap();

    let mut exposition = String::new();
    encode(&mut exposition, &registry).unwrap();
    assert!(
        exposition.contains("yggdrasil_shard_cache_capacity_evictions_total{artifact=\"graph\"} 1"),
        "capacity eviction is reported separately:\n{exposition}"
    );
    assert!(
        exposition.contains("yggdrasil_shard_cache_evictions_total{artifact=\"graph\"} 0"),
        "capacity pressure must not be reported as corruption:\n{exposition}"
    );
}

#[tokio::test]
async fn self_eviction_does_not_count_as_displacing_a_cached_artifact() {
    use prometheus_client::encoding::text::encode;
    use prometheus_client::registry::Registry;

    let store = Arc::new(ObservedStore::new());
    let pinned_revision = publish_graph(store.as_ref(), 49, "metric-pinned", 25).await;
    let rejected_revision = publish_graph(store.as_ref(), 49, "metric-rejected", 25).await;
    let pinned_size = store
        .object_size(&graph_segment_key(49, &pinned_revision))
        .await;
    let rejected_size = store
        .object_size(&graph_segment_key(49, &rejected_revision))
        .await;
    let capacity = CacheCapacity::new(pinned_size.max(rejected_size)).unwrap();
    let mut registry = Registry::default();
    let metrics = Metrics::registered(&mut registry);
    let directory = tempfile::tempdir().unwrap();
    let cache = ShardCache::with_metrics_and_capacity(store, directory.path(), metrics, capacity);

    let pinned = cache.leased_graph_path(49, &pinned_revision).await.unwrap();
    let error = cache
        .graph_path(49, &rejected_revision)
        .await
        .expect_err("the fresh artifact self-evicts while the cache is pinned");
    assert!(error.downcast_ref::<CacheCapacityUnavailable>().is_some());

    let mut exposition = String::new();
    encode(&mut exposition, &registry).unwrap();
    assert!(
        exposition.contains("yggdrasil_shard_cache_capacity_evictions_total{artifact=\"graph\"} 0"),
        "rejecting the just-fetched artifact is not a cache displacement:\n{exposition}"
    );
    drop(pinned);
}

#[tokio::test]
async fn fts_archive_and_unpacked_directory_are_one_bounded_bundle() {
    let store = Arc::new(ObservedStore::new());
    let revision = publish_graph(store.as_ref(), 47, "fts-bundle", 100).await;
    let archive_bytes = store.object_size(&fts_segment_key(47, &revision)).await;

    // First measure the complete bundle under the generous default. The
    // second cache is one byte too small for that archive+directory bundle,
    // while still large enough for the archive itself.
    let measurement_dir = tempfile::tempdir().unwrap();
    ShardCache::new(store.clone(), measurement_dir.path())
        .fts_path(47, &revision)
        .await
        .unwrap();
    let bundle_bytes = regular_file_bytes(measurement_dir.path());
    assert!(
        bundle_bytes > archive_bytes,
        "the fixture has unpacked files"
    );

    let capacity_bytes = bundle_bytes - 1;
    let bounded_dir = tempfile::tempdir().unwrap();
    let cache = ShardCache::with_capacity(
        store,
        bounded_dir.path(),
        CacheCapacity::new(capacity_bytes).unwrap(),
    );
    let err = cache
        .fts_path(47, &revision)
        .await
        .expect_err("the complete FTS bundle exceeds the cap");
    assert!(
        err.downcast_ref::<CacheCapacityUnavailable>().is_some()
            || err.downcast_ref::<CacheArtifactTooLarge>().is_some(),
        "oversized FTS accounting fails with a typed capacity error: {err:#}"
    );
    assert!(
        regular_file_bytes(bounded_dir.path()) <= capacity_bytes,
        "a rejected FTS bundle leaves no over-cap files"
    );
}

fn regular_file_bytes(root: &std::path::Path) -> u64 {
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
            let Ok(metadata) = entry.metadata() else {
                // A concurrent commit or eviction can remove a temp entry
                // between directory iteration and metadata inspection.
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                bytes += metadata.len();
            }
        }
    }
    bytes
}

#[test]
fn constructor_scan_removes_stale_temps_and_enforces_the_existing_disk_cap() {
    let directory = tempfile::tempdir().unwrap();
    let first = "1".repeat(64);
    let second = "2".repeat(64);
    std::fs::write(directory.path().join(format!("{first}.sqlite")), vec![0; 8]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(
        directory.path().join(format!("{second}.sqlite")),
        vec![0; 8],
    )
    .unwrap();
    let stale = directory
        .path()
        .join(format!("{second}.sqlite.tmp-123-456"));
    std::fs::write(&stale, vec![0; 32]).unwrap();
    let stale_fts = directory.path().join(format!("{second}.fts.tmp-123-457"));
    std::fs::create_dir(&stale_fts).unwrap();
    std::fs::write(stale_fts.join("partial-index"), vec![0; 32]).unwrap();

    let _cache = ShardCache::with_capacity(
        Arc::new(InMemory::new()),
        directory.path(),
        CacheCapacity::new(8).unwrap(),
    );

    assert!(!stale.exists(), "startup removes abandoned staging files");
    assert!(
        !stale_fts.exists(),
        "startup removes partially deleted FTS directories"
    );
    assert!(regular_file_bytes(directory.path()) <= 8);
    assert!(!directory.path().join(format!("{first}.sqlite")).exists());
    assert!(directory.path().join(format!("{second}.sqlite")).exists());
}
