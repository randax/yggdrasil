//! The query side's local Shard tier: warm queries answer without
//! object storage, and a cached segment that no longer matches its
//! checksum is refetched, never trusted.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use object_store::memory::InMemory;
use object_store::{GetOptions, GetResult, ObjectStore, PutOptions, PutPayload, PutResult};
use yg_shard::{
    Edge, EdgeKind, Graph, Node, NodeKind, Provenance, SearchDoc, SearchParams, ShardCache,
    delete_shard, published_shard, write_shard,
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
    let (graph, search_docs) = fixture_shard();
    write_shard(store, repo_id, commit, graph, search_docs)
        .await
        .expect("publishing the fixture Shard")
        .revision
}

fn fixture_shard() -> (Graph, Vec<SearchDoc>) {
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
    (graph, search_docs)
}

#[derive(Debug)]
struct DeletionSeamStore {
    inner: Arc<InMemory>,
    deletions: Arc<std::sync::Mutex<Vec<String>>>,
    delete_count: Arc<AtomicUsize>,
    fail_at: Option<usize>,
}

impl DeletionSeamStore {
    fn new(fail_at: Option<usize>) -> Self {
        Self {
            inner: Arc::new(InMemory::new()),
            deletions: Arc::new(std::sync::Mutex::new(Vec::new())),
            delete_count: Arc::new(AtomicUsize::new(0)),
            fail_at,
        }
    }

    fn deletions(&self) -> Vec<String> {
        self.deletions.lock().unwrap().clone()
    }
}

impl std::fmt::Display for DeletionSeamStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeletionSeamStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for DeletionSeamStore {
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
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        let inner = self.inner.clone();
        let deletions = self.deletions.clone();
        let delete_count = self.delete_count.clone();
        let fail_at = self.fail_at;
        locations
            .then(move |location| {
                let inner = inner.clone();
                let deletions = deletions.clone();
                let delete_count = delete_count.clone();
                async move {
                    let location = location?;
                    deletions.lock().unwrap().push(location.to_string());
                    let attempt = delete_count.fetch_add(1, Ordering::SeqCst) + 1;
                    if fail_at == Some(attempt) {
                        return Err(object_store::Error::Generic {
                            store: "deletion seam",
                            source: Box::new(std::io::Error::other("simulated GC crash")),
                        });
                    }
                    object_store::ObjectStoreExt::delete(inner.as_ref(), &location).await?;
                    Ok(location)
                }
            })
            .boxed()
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

#[tokio::test]
async fn deletion_removes_the_manifest_before_every_segment() {
    let store = DeletionSeamStore::new(None);
    let revision = publish_fixture_shard(&store, 7, "ordered").await;

    delete_shard(&store, 7, &revision).await.unwrap();

    let deletions = store.deletions();
    assert_eq!(
        deletions.first(),
        Some(&yg_shard::manifest_key(7, &revision))
    );
    for segment in [
        yg_shard::graph_segment_key(7, &revision),
        yg_shard::fts_segment_key(7, &revision),
    ] {
        assert!(
            deletions.iter().position(|key| key == &segment)
                > deletions
                    .iter()
                    .position(|key| key == &yg_shard::manifest_key(7, &revision)),
            "manifest deletion must precede {segment}: {deletions:?}"
        );
    }
}

#[tokio::test]
async fn a_partial_reclamation_is_fully_republished_and_served() {
    use object_store::ObjectStoreExt;

    let store = Arc::new(DeletionSeamStore::new(Some(3)));
    let commit = "repairable";
    let revision = publish_fixture_shard(store.as_ref(), 9, commit).await;
    let error = delete_shard(store.as_ref(), 9, &revision)
        .await
        .expect_err("the injected deletion failure must interrupt reclamation");
    assert!(error.to_string().contains("deleting Shard objects"));
    assert!(matches!(
        store
            .get(&yg_shard::manifest_key(9, &revision).as_str().into())
            .await,
        Err(object_store::Error::NotFound { .. })
    ));

    let (graph, search_docs) = fixture_shard();
    write_shard(store.as_ref(), 9, commit, graph, search_docs)
        .await
        .expect("the next index run repairs the partial deletion");
    let dir = tempfile::tempdir().unwrap();
    let cache = ShardCache::new(store, dir.path());
    let graph_path = cache.graph_path(9, &revision).await.unwrap();
    assert!(graph_path.is_file());
    let fts_path = cache.fts_path(9, &revision).await.unwrap();
    let index = yg_shard::open_fts(&fts_path).unwrap();
    assert!(
        !yg_shard::search(
            &index,
            &SearchParams {
                query: "Hello",
                kinds: None,
                limit: 10,
            },
        )
        .unwrap()
        .is_empty()
    );
}

#[tokio::test]
async fn a_warm_cache_refreshes_a_manifest_after_same_revision_republish() {
    let store = Arc::new(InMemory::new());
    let commit = "warm-repair";
    let revision = publish_fixture_shard(store.as_ref(), 11, commit).await;
    let dir = tempfile::tempdir().unwrap();
    let cache = ShardCache::new(store.clone(), dir.path());

    cache
        .graph_path(11, &revision)
        .await
        .expect("warm the original manifest without fetching full text");
    delete_shard(store.as_ref(), 11, &revision).await.unwrap();
    let (graph, mut search_docs) = fixture_shard();
    search_docs.push(SearchDoc {
        node_id: "sym:main.go#Repaired".into(),
        kind: NodeKind::Symbol,
        name: Some("Repaired".into()),
        path: Some("main.go".into()),
        content: "Repaired".into(),
    });
    write_shard(store.as_ref(), 11, commit, graph, search_docs)
        .await
        .unwrap();

    let fts_path = cache.fts_path(11, &revision).await.unwrap();
    let index = yg_shard::open_fts(&fts_path).unwrap();
    let hits = yg_shard::search(
        &index,
        &SearchParams {
            query: "Repaired",
            kinds: None,
            limit: 10,
        },
    )
    .unwrap();
    assert!(
        hits.iter()
            .any(|hit| hit.name.as_deref() == Some("Repaired"))
    );
}

#[tokio::test]
async fn a_manifest_with_a_missing_listed_segment_is_not_published() {
    use object_store::ObjectStoreExt;

    let store = InMemory::new();
    let commit = "incomplete";
    let revision = publish_fixture_shard(&store, 3, commit).await;
    store
        .delete(&yg_shard::fts_segment_key(3, &revision).as_str().into())
        .await
        .unwrap();

    assert!(published_shard(&store, 3, commit).await.unwrap().is_none());
}

#[tokio::test]
async fn every_manifest_listed_segment_must_exist_before_publication_is_trusted() {
    use object_store::ObjectStoreExt;

    let store = InMemory::new();
    let commit = "extra-segment";
    let revision = publish_fixture_shard(&store, 4, commit).await;
    let key = yg_shard::manifest_key(4, &revision);
    let bytes = store
        .get(&key.as_str().into())
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    manifest["segments"]["symbols.sqlite"] = serde_json::json!({
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bytes": 1
    });
    store
        .put(
            &key.as_str().into(),
            serde_json::to_vec(&manifest).unwrap().into(),
        )
        .await
        .unwrap();

    assert!(published_shard(&store, 4, commit).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_shard_only_deletes_the_exact_revision_directory() {
    use object_store::ObjectStoreExt;
    use object_store::path::Path;

    let store = InMemory::new();
    let doomed = "abc";
    let neighbor = "abc-suffix";
    for key in [
        yg_shard::manifest_key(7, doomed),
        yg_shard::graph_segment_key(7, doomed),
        yg_shard::manifest_key(7, neighbor),
        yg_shard::graph_segment_key(7, neighbor),
    ] {
        store
            .put(&Path::from(key), "segment".to_string().into())
            .await
            .unwrap();
    }

    delete_shard(&store, 7, doomed).await.unwrap();

    for key in [
        yg_shard::manifest_key(7, doomed),
        yg_shard::graph_segment_key(7, doomed),
    ] {
        assert!(
            matches!(
                store.get(&Path::from(key)).await,
                Err(object_store::Error::NotFound { .. })
            ),
            "the reclaimed revision's object must be gone"
        );
    }
    for key in [
        yg_shard::manifest_key(7, neighbor),
        yg_shard::graph_segment_key(7, neighbor),
    ] {
        store
            .get(&Path::from(key))
            .await
            .unwrap_or_else(|e| panic!("neighbor revision object must survive: {e}"));
    }
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
async fn a_cached_full_text_segment_failing_its_checksum_is_refetched_not_trusted() {
    let store = Arc::new(CountingStore::new());
    let revision = publish_fixture_shard(store.as_ref(), 1, "abc123").await;
    let dir = tempfile::tempdir().unwrap();

    // Materialize once, then vandalize the cached archive and drop the
    // unpacked dir — a restarted query node must notice, refetch, re-unpack.
    let unpacked = ShardCache::new(store.clone(), dir.path())
        .fts_path(1, &revision)
        .await
        .unwrap();
    let sha = unpacked
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".fts"))
        .expect("the unpacked segment dir is named <sha>.fts")
        .to_string();
    let archive = dir.path().join(format!("{sha}.tar"));
    let pristine = std::fs::read(&archive).unwrap();
    std::fs::write(&archive, b"flipped bits, not a tantivy segment").unwrap();
    std::fs::remove_dir_all(&unpacked).unwrap();

    let gets_before = store.gets();
    let repaired = ShardCache::new(store.clone(), dir.path())
        .fts_path(1, &revision)
        .await
        .unwrap();
    assert_eq!(repaired, unpacked, "the same revision maps to the same dir");
    assert_eq!(
        std::fs::read(&archive).unwrap(),
        pristine,
        "the corrupt archive must be replaced by the artifact from storage"
    );
    assert!(
        store.gets() > gets_before,
        "a checksum mismatch must refetch from storage"
    );

    // The healed segment is a working tantivy index again.
    let index = yg_shard::open_fts(&repaired).expect("the re-unpacked segment opens");
    let hits = yg_shard::search(
        &index,
        &SearchParams {
            query: "Hello",
            kinds: None,
            limit: 10,
        },
    )
    .expect("search runs over the healed segment");
    assert!(
        hits.iter().any(|h| h.node_id == "sym:main.go#Hello"),
        "the healed segment finds the indexed symbol: {hits:?}"
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
