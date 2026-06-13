//! The query side's local Shard tier: warm queries answer without
//! object storage, and a cached segment that no longer matches its
//! checksum is refetched, never trusted.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use object_store::memory::InMemory;
use object_store::{GetOptions, GetResult, ObjectStore, PutOptions, PutPayload, PutResult};
use yg_shard::{
    Edge, EdgeKind, Graph, Node, NodeKind, Provenance, SearchDoc, SearchParams, ShardCache,
    write_shard,
};

/// An object store that counts reads, so tests can assert which queries
/// went to storage and which the local tier answered.
#[derive(Debug)]
struct CountingStore {
    inner: InMemory,
    gets: AtomicUsize,
    /// Every key fetched, in order — for asserting *what* was read,
    /// not just how often.
    fetched: std::sync::Mutex<Vec<String>>,
}

impl CountingStore {
    fn new() -> Self {
        Self {
            inner: InMemory::new(),
            gets: AtomicUsize::new(0),
            fetched: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }

    fn fetched(&self) -> Vec<String> {
        self.fetched.lock().unwrap().clone()
    }
}

impl std::fmt::Display for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CountingStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for CountingStore {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.fetched.lock().unwrap().push(location.to_string());
        self.inner.get_opts(location, options).await
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

/// A tiny published Shard to read back: one File defining one Symbol.
async fn publish_fixture_shard(store: &dyn ObjectStore, repo_id: i64, commit: &str) -> String {
    let graph = Graph {
        nodes: vec![Node::file("main.go"), Node::symbol("main.go", "Hello", 1)],
        edges: vec![Edge {
            src: "file:main.go".into(),
            dst: "sym:main.go#Hello".into(),
            kind: EdgeKind::Defines,
            provenance: Provenance::Syntactic,
            confidence: 0.9,
            location: None,
        }],
    };
    let search_docs = vec![
        SearchDoc {
            node_id: "file:main.go".into(),
            kind: NodeKind::File,
            name: Some("main.go".into()),
            path: Some("main.go".into()),
            content: "package main\n\nfunc Hello() string { return \"hi\" }\n".into(),
        },
        SearchDoc {
            node_id: "sym:main.go#Hello".into(),
            kind: NodeKind::Symbol,
            name: Some("Hello".into()),
            path: Some("main.go".into()),
            content: String::new(),
        },
    ];
    write_shard(store, repo_id, commit, graph, search_docs)
        .await
        .expect("publishing the fixture Shard")
        .revision
}

#[tokio::test]
async fn warm_queries_answer_from_the_local_tier_without_object_storage() {
    let store = Arc::new(CountingStore::new());
    let revision = publish_fixture_shard(store.as_ref(), 1, "abc123").await;
    let dir = tempfile::tempdir().unwrap();
    let cache = ShardCache::new(store.clone(), dir.path());

    let cold = cache.graph_path(1, &revision).await.unwrap();
    let cold_gets = store.gets();
    assert!(cold_gets > 0, "a cold query must fetch from storage");

    for _ in 0..3 {
        let warm = cache.graph_path(1, &revision).await.unwrap();
        assert_eq!(warm, cold, "the same revision maps to the same file");
    }
    assert_eq!(
        store.gets(),
        cold_gets,
        "warm queries must not touch object storage"
    );
}

#[tokio::test]
async fn the_full_text_segment_materializes_once_and_then_searches_warm() {
    let store = Arc::new(CountingStore::new());
    let revision = publish_fixture_shard(store.as_ref(), 1, "abc123").await;
    let dir = tempfile::tempdir().unwrap();
    let cache = ShardCache::new(store.clone(), dir.path());

    let cold = cache.fts_path(1, &revision).await.unwrap();
    let cold_gets = store.gets();
    assert!(cold_gets > 0, "a cold query must fetch from storage");

    for _ in 0..3 {
        let warm = cache.fts_path(1, &revision).await.unwrap();
        assert_eq!(warm, cold, "the same revision maps to the same directory");
    }
    assert_eq!(
        store.gets(),
        cold_gets,
        "warm queries must not touch object storage"
    );

    // The materialized segment is a working tantivy index: the fixture's
    // Hello symbol is findable, and its node id feeds the other Verbs.
    let index = yg_shard::open_fts(&cold).expect("the unpacked segment opens");
    let hits = yg_shard::search(
        &index,
        &SearchParams {
            query: "Hello",
            kinds: None,
            limit: 10,
        },
    )
    .expect("search runs over the cached segment");
    assert!(
        hits.iter().any(|h| h.node_id == "sym:main.go#Hello"),
        "the cached segment finds the indexed symbol: {hits:?}"
    );
}

#[tokio::test]
async fn a_cached_segment_failing_its_checksum_is_refetched_not_trusted() {
    let store = Arc::new(CountingStore::new());
    let revision = publish_fixture_shard(store.as_ref(), 1, "abc123").await;
    let dir = tempfile::tempdir().unwrap();

    let path = ShardCache::new(store.clone(), dir.path())
        .graph_path(1, &revision)
        .await
        .unwrap();
    let pristine = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"flipped bits, not a graph segment").unwrap();

    // A fresh cache (a restarted query node) must notice and repair.
    let gets_before = store.gets();
    let repaired = ShardCache::new(store.clone(), dir.path())
        .graph_path(1, &revision)
        .await
        .unwrap();
    assert_eq!(repaired, path);
    assert_eq!(
        std::fs::read(&repaired).unwrap(),
        pristine,
        "the corrupt file must be replaced by the artifact from storage"
    );
    assert!(
        store.gets() > gets_before,
        "a checksum mismatch must refetch from storage"
    );
}

#[tokio::test]
async fn a_restarted_cache_reuses_an_intact_segment_without_refetching_it() {
    let store = Arc::new(CountingStore::new());
    let revision = publish_fixture_shard(store.as_ref(), 1, "abc123").await;
    let dir = tempfile::tempdir().unwrap();

    ShardCache::new(store.clone(), dir.path())
        .graph_path(1, &revision)
        .await
        .unwrap();
    let segment_key = yg_shard::graph_segment_key(1, &revision);
    let fetches_before = store.fetched().len();

    // A restarted query node re-reads the manifest (cheap) but must
    // verify and reuse the segment already on disk.
    ShardCache::new(store.clone(), dir.path())
        .graph_path(1, &revision)
        .await
        .unwrap();
    let after_restart = &store.fetched()[fetches_before..];
    assert_eq!(
        after_restart.len(),
        1,
        "only one refetch after a restart: {after_restart:?}"
    );
    assert_ne!(
        after_restart[0], segment_key,
        "the one refetch is the manifest, never the segment"
    );
}

#[tokio::test]
async fn a_manifest_recording_a_non_checksum_segment_name_is_refused() {
    use object_store::ObjectStoreExt;
    let store = Arc::new(CountingStore::new());
    let commit = "abc123";
    let revision = yg_shard::syntactic_revision(commit);

    // A doctored manifest: consistent with its revision, but its
    // segment "checksum" tries to reach outside the cache directory.
    let manifest = serde_json::json!({
        "schema_version": yg_shard::SCHEMA_VERSION,
        "commit": commit,
        "pass": yg_shard::SYNTACTIC_PASS,
        "counts": {"nodes": 0, "edges": 0},
        "segments": {"graph.sqlite": {"sha256": "../../../tmp/evil", "bytes": 1}},
    });
    store
        .put(
            &yg_shard::manifest_key(1, &revision).as_str().into(),
            manifest.to_string().into(),
        )
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let err = ShardCache::new(store.clone(), dir.path())
        .graph_path(1, &revision)
        .await
        .expect_err("a non-checksum segment name must be refused");
    assert!(
        err.to_string().contains("not a sha256 digest"),
        "names the reason: {err:#}"
    );
}

#[tokio::test]
async fn a_shard_from_an_older_schema_is_refused_with_a_typed_error() {
    use object_store::ObjectStoreExt;
    let store = Arc::new(CountingStore::new());
    let commit = "abc123";

    // A manifest honestly describing an older-schema revision — exactly
    // what a pre-deploy Shard looks like to a binary that has since
    // bumped SCHEMA_VERSION. It agrees with its own revision id, so the
    // disagreement check passes; only the schema gate may stop it.
    let older = yg_shard::SCHEMA_VERSION - 1;
    let revision = format!("{commit}-{}-v{older}", yg_shard::SYNTACTIC_PASS);
    let manifest = serde_json::json!({
        "schema_version": older,
        "commit": commit,
        "pass": yg_shard::SYNTACTIC_PASS,
        "counts": {"nodes": 0, "edges": 0},
        "segments": {"graph.sqlite": {"sha256": "0".repeat(64), "bytes": 1}},
    });
    store
        .put(
            &yg_shard::manifest_key(1, &revision).as_str().into(),
            manifest.to_string().into(),
        )
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let err = ShardCache::new(store.clone(), dir.path())
        .graph_path(1, &revision)
        .await
        .expect_err("an older-schema Shard must be refused, not read");
    let outdated = err
        .downcast_ref::<yg_shard::SchemaOutdated>()
        .unwrap_or_else(|| panic!("must be a typed SchemaOutdated, got: {err:#}"));
    assert_eq!(outdated.schema_version, older, "carries the stale version");
    // The gate fires on the manifest alone — the segment is never read,
    // so its missing-column SQL never gets a chance to fail cryptically.
    assert!(
        !store
            .fetched()
            .iter()
            .any(|key| key.ends_with(yg_shard::GRAPH_SEGMENT_FILE)),
        "the graph segment must not be fetched once the schema is stale"
    );
}

#[tokio::test]
async fn a_stale_revision_is_fetched_at_most_once_per_process() {
    use object_store::ObjectStoreExt;
    let store = Arc::new(CountingStore::new());
    let commit = "abc123";

    // An honest older-schema manifest, as a deploy rollout leaves behind
    // until re-indexing converges.
    let older = yg_shard::SCHEMA_VERSION - 1;
    let revision = format!("{commit}-{}-v{older}", yg_shard::SYNTACTIC_PASS);
    let manifest = serde_json::json!({
        "schema_version": older,
        "commit": commit,
        "pass": yg_shard::SYNTACTIC_PASS,
        "counts": {"nodes": 0, "edges": 0},
        "segments": {"graph.sqlite": {"sha256": "0".repeat(64), "bytes": 1}},
    });
    store
        .put(
            &yg_shard::manifest_key(1, &revision).as_str().into(),
            manifest.to_string().into(),
        )
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cache = ShardCache::new(store.clone(), dir.path());

    // A hot repo mid-rollout: many queries hit the same stale revision.
    // Each must still be refused — but the manifest is fetched once and
    // the verdict served from the warm cache, not refetched per query.
    for _ in 0..5 {
        let err = cache
            .graph_path(1, &revision)
            .await
            .expect_err("a stale revision stays refused");
        assert!(err.downcast_ref::<yg_shard::SchemaOutdated>().is_some());
    }
    let manifest_fetches = store
        .fetched()
        .iter()
        .filter(|key| key.ends_with("manifest.json"))
        .count();
    assert_eq!(
        manifest_fetches,
        1,
        "the stale manifest is fetched once, then served from cache: {:?}",
        store.fetched()
    );
}
