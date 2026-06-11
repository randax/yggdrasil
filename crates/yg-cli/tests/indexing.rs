//! Indexing pipeline behavior: the syntactic pass over a synced checkout
//! publishes an immutable Shard and surfaces it in `yg admin status`.
//! Runs against the dev compose stack like e2e.rs (see docs/DEVELOPMENT.md).

mod common;

use common::*;
use yg_api::serve;

/// A Go repo fixture standing in for a Forge-hosted one: two functions in
/// main.go plus a README. Returns (fixture root guard, repo dir, URL).
fn go_fixture_repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("fixtures/acme/gadgets");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "fixture@example.com"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(
        repo.join("main.go"),
        "package main\n\nfunc Hello() string {\n\treturn \"hello\"\n}\n\nfunc main() {\n\tprintln(Hello())\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("README.md"), "# gadgets\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);
    let url = format!("file://{}", repo.display());
    (root, repo, url)
}

/// A booted server plus sync and index workers around one Go fixture
/// repo: everything an indexing test drives.
struct Harness {
    fixture: tempfile::TempDir,
    repo_dir: std::path::PathBuf,
    fixture_url: String,
    db_name: String,
    base: String,
    _server: yg_api::RunningServer,
    sync: yg_sync::SyncWorker,
    indexer: yg_index::IndexWorker,
    store: std::sync::Arc<dyn object_store::ObjectStore>,
}

impl Harness {
    async fn boot() -> Self {
        let (fixture, repo_dir, fixture_url) = go_fixture_repo();
        let cache = fixture.path().join("git-cache");
        let db_name = create_test_db().await;
        let config = test_config(&db_name);
        let store = config
            .object_store
            .connect()
            .expect("dev MinIO must be reachable");
        let server = serve(config).await.expect("boot");
        let base = format!("http://{}", server.local_addr());
        let sync = yg_sync::SyncWorker::new(control_plane(&db_name).await, &cache);
        let indexer =
            yg_index::IndexWorker::new(control_plane(&db_name).await, store.clone(), &cache);
        Self {
            fixture,
            repo_dir,
            fixture_url,
            db_name,
            base,
            _server: server,
            sync,
            indexer,
            store,
        }
    }

    async fn add_repo(&self) {
        post_repo(&self.base, serde_json::json!({"url": self.fixture_url})).await;
    }

    /// Drive one fetch + one index through the workers.
    async fn sync_and_index(&self) {
        assert!(self.sync.run_once().await.unwrap(), "fetch job must run");
        assert!(
            self.indexer
                .run_once()
                .await
                .expect("indexing must not error"),
            "a successful fetch must queue an index job"
        );
    }

    /// The repo's `shard` object from admin status.
    async fn shard_status(&self) -> serde_json::Value {
        admin_status_body(&self.base).await["repos"][0]["shard"].clone()
    }
}

#[tokio::test]
async fn indexing_a_synced_go_repo_publishes_a_shard_with_symbols_and_counts() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let shard = h.shard_status().await;
    assert!(
        shard["revision"].as_str().is_some_and(|r| !r.is_empty()),
        "an indexed repo must show its Shard revision, got: {shard}"
    );
    // main.go and README.md are File nodes; Hello and main are Symbols,
    // each defined by main.go.
    assert_eq!(shard["nodes"], 4, "got: {shard}");
    assert_eq!(shard["edges"], 2, "got: {shard}");

    assert!(
        !h.indexer.run_once().await.unwrap(),
        "the index queue must be drained after the one job"
    );
}

#[tokio::test]
async fn re_indexing_the_same_commit_is_idempotent() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let first = h.shard_status().await;

    // The repo hasn't moved; a re-add re-syncs the same commit and the
    // pipeline indexes it again.
    h.add_repo().await;
    h.sync_and_index().await;

    let second = h.shard_status().await;
    assert_eq!(
        first, second,
        "re-indexing an unchanged commit must change nothing"
    );

    // Idempotent means no second revision was recorded, not merely that
    // the pointer looks the same.
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", h.db_name))
        .await
        .unwrap();
    let (revisions,): (i64,) = sqlx::query_as("SELECT count(*) FROM shards")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(revisions, 1, "one commit, one Shard revision");
}

#[tokio::test]
async fn a_new_commit_publishes_a_new_revision_and_never_mutates_the_old_shard() {
    use object_store::ObjectStoreExt;

    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let first = h.shard_status().await;

    // Pin the first Shard's bytes before the repo moves on.
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", h.db_name))
        .await
        .unwrap();
    let (manifest_key,): (String,) = sqlx::query_as("SELECT manifest_key FROM shards")
        .fetch_one(&pool)
        .await
        .unwrap();
    let graph_key = manifest_key.replace("manifest.json", "graph.sqlite");
    let bytes_of = |key: String| {
        let store = h.store.clone();
        async move {
            store
                .get(&key.as_str().into())
                .await
                .expect("shard object must exist")
                .bytes()
                .await
                .unwrap()
        }
    };
    let first_manifest = bytes_of(manifest_key.clone()).await;
    let first_graph = bytes_of(graph_key.clone()).await;

    // The repo grows a function; sync and index pick it up.
    std::fs::write(
        h.repo_dir.join("main.go"),
        "package main\n\nfunc Hello() string {\n\treturn \"hello\"\n}\n\nfunc Bye() string {\n\treturn \"bye\"\n}\n\nfunc main() {\n\tprintln(Hello())\n}\n",
    )
    .unwrap();
    git(&h.repo_dir, &["add", "."]);
    git(&h.repo_dir, &["commit", "-m", "add Bye"]);
    h.add_repo().await;
    h.sync_and_index().await;

    let second = h.shard_status().await;
    assert_ne!(
        second["revision"], first["revision"],
        "a new commit must publish a new Shard revision"
    );
    assert_eq!(second["nodes"], 5, "the new Shard sees Bye, got: {second}");
    assert_eq!(second["edges"], 3, "got: {second}");

    // Both revisions are recorded; the old Shard's objects are untouched.
    let (revisions,): (i64,) = sqlx::query_as("SELECT count(*) FROM shards")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(revisions, 2, "every published revision stays recorded");
    assert_eq!(
        bytes_of(manifest_key).await,
        first_manifest,
        "an existing Shard's manifest must never change"
    );
    assert_eq!(
        bytes_of(graph_key).await,
        first_graph,
        "an existing Shard's graph segment must never change"
    );
}

#[tokio::test]
async fn the_manifest_records_commit_checksums_counts_and_schema_version() {
    use object_store::ObjectStoreExt;
    use sha2::Digest;

    let h = Harness::boot().await;
    let head = git(&h.repo_dir, &["rev-parse", "HEAD"]);
    h.add_repo().await;
    h.sync_and_index().await;

    let shard = h.shard_status().await;
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", h.db_name))
        .await
        .unwrap();
    let (manifest_key,): (String,) = sqlx::query_as("SELECT manifest_key FROM shards")
        .fetch_one(&pool)
        .await
        .unwrap();

    let manifest_bytes = h
        .store
        .get(&manifest_key.as_str().into())
        .await
        .expect("the manifest must be in object storage")
        .bytes()
        .await
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();

    assert_eq!(
        manifest["commit"].as_str(),
        Some(head.as_str()),
        "the manifest must name the indexed commit, got: {manifest}"
    );
    assert_eq!(manifest["schema_version"], yg_shard::SCHEMA_VERSION);
    assert_eq!(manifest["pass"], "syntactic");
    assert_eq!(
        manifest["counts"]["nodes"], shard["nodes"],
        "manifest and status must agree on counts"
    );
    assert_eq!(manifest["counts"]["edges"], shard["edges"]);

    // The recorded checksum must verify the stored segment.
    let segment = &manifest["segments"]["graph.sqlite"];
    let graph_bytes = h
        .store
        .get(
            &manifest_key
                .replace("manifest.json", "graph.sqlite")
                .as_str()
                .into(),
        )
        .await
        .expect("the graph segment must be in object storage")
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        segment["sha256"].as_str(),
        Some(hex::encode(sha2::Sha256::digest(&graph_bytes)).as_str()),
        "the manifest checksum must match the stored graph segment"
    );
    assert_eq!(segment["bytes"], graph_bytes.len() as u64);
}

#[tokio::test]
async fn every_edge_row_carries_provenance_and_confidence() {
    use object_store::ObjectStoreExt;

    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", h.db_name))
        .await
        .unwrap();
    let (manifest_key,): (String,) = sqlx::query_as("SELECT manifest_key FROM shards")
        .fetch_one(&pool)
        .await
        .unwrap();
    let graph_bytes = h
        .store
        .get(
            &manifest_key
                .replace("manifest.json", "graph.sqlite")
                .as_str()
                .into(),
        )
        .await
        .expect("the graph segment must be in object storage")
        .bytes()
        .await
        .unwrap();

    // The graph segment is a queryable SQLite file (RFC 0001 §6).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.sqlite");
    std::fs::write(&path, &graph_bytes).unwrap();
    let graph =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();

    // The schema itself must demand the fields (ADR 0002: carried from
    // day one), not merely happen to fill them.
    let required_not_null: Vec<(String, bool)> = graph
        .prepare("SELECT name, \"notnull\" FROM pragma_table_info('edges') WHERE name IN ('provenance', 'confidence')")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get::<_, i64>(1)? == 1)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        required_not_null.len(),
        2,
        "edges must have provenance and confidence columns"
    );
    for (column, not_null) in required_not_null {
        assert!(not_null, "{column} must be NOT NULL in the edge schema");
    }

    // And every row must carry meaningful values: a known provenance and
    // a confidence in (0, 1].
    let edges: Vec<(String, String, String, f64)> = graph
        .prepare("SELECT src, dst, provenance, confidence FROM edges")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!edges.is_empty(), "the Go fixture must yield edges");
    for (src, dst, provenance, confidence) in edges {
        assert_eq!(
            provenance, "syntactic",
            "M0 edges all come from the syntactic pass ({src} → {dst})"
        );
        assert!(
            confidence > 0.0 && confidence <= 1.0,
            "confidence must be in (0, 1], got {confidence} on {src} → {dst}"
        );
    }
}
