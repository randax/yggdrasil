//! A pinned artifact stays in the cache for its whole read and becomes
//! evictable however that read ends. Each fixture has capacity for exactly
//! one artifact, so a competing fetch observes the pin.

use std::sync::Arc;
use std::sync::mpsc;

use object_store::memory::InMemory;
use object_store::{ObjectStore, ObjectStoreExt};
use tokio::sync::oneshot;
use yg_shard::{
    CacheCapacity, CacheCapacityUnavailable, Edge, Graph, Node, NodeKind, PinnedArtifact,
    SearchDoc, ShardCache, graph_segment_key, write_shard,
};

const REPO: i64 = 71;

async fn publish(store: &dyn ObjectStore, label: &str) -> String {
    let nodes = (0..25)
        .map(|index| Node::file(&format!("{label}/{index:04}.rs")))
        .collect();
    let graph = Graph {
        nodes,
        edges: Vec::<Edge>::new(),
    };
    // Distinct content: the cache is content-addressed, so identical segments
    // would share one artifact and could never compete for capacity.
    let docs = vec![SearchDoc {
        node_id: format!("file:{label}.md"),
        kind: NodeKind::File,
        name: Some(format!("{label}.md")),
        path: Some(format!("{label}.md")),
        content: label.to_string(),
    }];
    write_shard(store, REPO, label, graph, docs)
        .await
        .expect("publish fixture")
        .revision
}

async fn object_size(store: &dyn ObjectStore, key: &str) -> u64 {
    store.head(&key.into()).await.expect("head object").size
}

fn directory_bytes(root: &std::path::Path) -> u64 {
    std::fs::read_dir(root)
        .expect("read cache directory")
        .flatten()
        .map(|entry| {
            let metadata = entry.metadata().expect("cache entry metadata");
            if metadata.is_dir() {
                directory_bytes(&entry.path())
            } else {
                metadata.len()
            }
        })
        .sum()
}

struct Fixture {
    cache: ShardCache,
    first: String,
    second: String,
    _directory: tempfile::TempDir,
}

/// A cache with room for either graph segment, but not both.
async fn graph_fixture() -> Fixture {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let first = publish(store.as_ref(), "first").await;
    let second = publish(store.as_ref(), "other").await;
    let capacity = object_size(store.as_ref(), graph_segment_key(REPO, &first).as_str())
        .await
        .max(object_size(store.as_ref(), graph_segment_key(REPO, &second).as_str()).await);
    let directory = tempfile::tempdir().expect("cache directory");
    let cache = ShardCache::with_capacity(
        store,
        directory.path(),
        CacheCapacity::new(capacity).expect("non-zero capacity"),
    );
    Fixture {
        cache,
        first,
        second,
        _directory: directory,
    }
}

/// A cache with room for either complete FTS bundle, but not both.
async fn fts_fixture() -> Fixture {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let first = publish(store.as_ref(), "first").await;
    let second = publish(store.as_ref(), "other").await;
    let mut capacity = 0;
    for revision in [&first, &second] {
        let measuring = tempfile::tempdir().expect("measurement directory");
        ShardCache::new(store.clone(), measuring.path())
            .fts_path(REPO, revision)
            .await
            .expect("measure FTS bundle");
        capacity = capacity.max(directory_bytes(measuring.path()));
    }
    let directory = tempfile::tempdir().expect("cache directory");
    let cache = ShardCache::with_capacity(
        store,
        directory.path(),
        CacheCapacity::new(capacity).expect("non-zero capacity"),
    );
    Fixture {
        cache,
        first,
        second,
        _directory: directory,
    }
}

fn assert_capacity_contention(error: &anyhow::Error) {
    assert!(
        error.downcast_ref::<CacheCapacityUnavailable>().is_some(),
        "the pin must hold the artifact in the cache: {error:#}"
    );
}

#[tokio::test]
async fn a_graph_read_keeps_its_artifact_pinned_until_it_finishes() {
    let fixture = graph_fixture().await;
    let artifact: PinnedArtifact = fixture
        .cache
        .leased_graph_path(REPO, &fixture.first)
        .await
        .expect("pin first artifact")
        .into();
    let (entered, reading) = oneshot::channel();
    let (finish, finished) = mpsc::channel::<()>();

    let read = tokio::spawn(artifact.read_graph(move |connection| {
        entered.send(()).expect("test is waiting");
        finished.recv().expect("test releases the read");
        Ok(connection.query_row("SELECT 7", [], |row| row.get::<_, i64>(0))?)
    }));
    reading.await.expect("read started");

    let contention = fixture
        .cache
        .graph_path(REPO, &fixture.second)
        .await
        .expect_err("a pinned artifact must not be evicted mid-read");
    assert_capacity_contention(&contention);

    finish.send(()).expect("read is waiting");
    let value = read.await.expect("task").expect("no panic").expect("read");
    assert_eq!(value, 7);

    fixture
        .cache
        .graph_path(REPO, &fixture.second)
        .await
        .expect("the finished read released its pin");
}

#[tokio::test]
async fn an_fts_read_keeps_its_artifact_pinned_until_it_finishes() {
    let fixture = fts_fixture().await;
    let artifact: PinnedArtifact = fixture
        .cache
        .leased_fts_path(REPO, &fixture.first)
        .await
        .expect("pin first artifact")
        .into();
    let (entered, reading) = oneshot::channel();
    let (finish, finished) = mpsc::channel::<()>();

    let read = tokio::spawn(artifact.read_fts(move |_index| {
        entered.send(()).expect("test is waiting");
        finished.recv().expect("test releases the read");
        Ok(())
    }));
    reading.await.expect("read started");

    let contention = fixture
        .cache
        .fts_path(REPO, &fixture.second)
        .await
        .expect_err("a pinned artifact must not be evicted mid-read");
    assert_capacity_contention(&contention);

    finish.send(()).expect("read is waiting");
    read.await.expect("task").expect("no panic").expect("read");

    fixture
        .cache
        .fts_path(REPO, &fixture.second)
        .await
        .expect("the finished read released its pin");
}

#[tokio::test]
async fn a_failed_read_reports_its_error_and_releases_the_pin() {
    let fixture = graph_fixture().await;
    let artifact: PinnedArtifact = fixture
        .cache
        .leased_graph_path(REPO, &fixture.first)
        .await
        .expect("pin first artifact")
        .into();

    let outcome = artifact
        .read_graph(|_connection| -> anyhow::Result<()> { anyhow::bail!("query failed") })
        .await
        .expect("no panic");

    assert_eq!(
        outcome.expect_err("the read failed").to_string(),
        "query failed"
    );
    fixture
        .cache
        .graph_path(REPO, &fixture.second)
        .await
        .expect("the failed read released its pin");
}

#[tokio::test]
async fn a_panicking_read_reports_the_panic_and_releases_the_pin() {
    let fixture = graph_fixture().await;
    let artifact: PinnedArtifact = fixture
        .cache
        .leased_graph_path(REPO, &fixture.first)
        .await
        .expect("pin first artifact")
        .into();

    let outcome = artifact
        .read_graph(|_connection| -> anyhow::Result<()> { panic!("read panicked") })
        .await;

    assert!(outcome.expect_err("the read panicked").is_panic());
    fixture
        .cache
        .graph_path(REPO, &fixture.second)
        .await
        .expect("the panicked read released its pin");
}

#[tokio::test]
async fn an_artifact_that_cannot_be_opened_reports_a_read_error() {
    let directory = tempfile::tempdir().expect("directory");
    let missing = directory.path().join("missing");

    let graph = PinnedArtifact::unpinned(missing.clone())
        .read_graph(|_connection| Ok(()))
        .await
        .expect("no panic");
    let fts = PinnedArtifact::unpinned(missing)
        .read_fts(|_index| Ok(()))
        .await
        .expect("no panic");

    assert!(graph.is_err(), "a missing graph segment cannot be read");
    assert!(fts.is_err(), "a missing FTS segment cannot be read");
}
