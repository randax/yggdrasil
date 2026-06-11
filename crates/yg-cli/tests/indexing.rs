//! Indexing pipeline behavior: the syntactic pass over a synced checkout
//! publishes an immutable Shard and surfaces it in `yg admin status`.
//! Runs against the dev compose stack like e2e.rs (see docs/DEVELOPMENT.md).

mod common;

use common::*;
use yg_api::serve;

/// A Go repo fixture standing in for a Forge-hosted one: two functions in
/// main.go plus a README. Returns (fixture root guard, repo dir, URL).
fn go_fixture_repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
    fixture_repo_with(&[
        (
            "main.go",
            "package main\n\nfunc Hello() string {\n\treturn \"hello\"\n}\n\nfunc main() {\n\tprintln(Hello())\n}\n",
        ),
        ("README.md", "# gadgets\n"),
    ])
}

/// A booted server plus sync and index workers around one Go fixture
/// repo: everything an indexing test drives.
struct Harness {
    /// Held for Drop: owns the fixture repo and git cache on disk.
    _fixture: tempfile::TempDir,
    repo_dir: std::path::PathBuf,
    cache: std::path::PathBuf,
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
            _fixture: fixture,
            repo_dir,
            cache,
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

    /// A pool on the harness database, for SQL-level assertions.
    async fn pool(&self) -> sqlx::PgPool {
        sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", self.db_name))
            .await
            .unwrap()
    }

    /// The recorded Shard's manifest key (single-shard tests only).
    async fn manifest_key(&self) -> String {
        let (key,): (String,) = sqlx::query_as("SELECT manifest_key FROM shards")
            .fetch_one(&self.pool().await)
            .await
            .unwrap();
        key
    }

    /// The recorded Shard's graph-segment key, derived the same way the
    /// reader will: beside its manifest.
    async fn graph_key(&self) -> String {
        self.manifest_key()
            .await
            .replace("manifest.json", "graph.sqlite")
    }

    /// Raw bytes of one Shard object.
    async fn object_bytes(&self, key: &str) -> Vec<u8> {
        use object_store::ObjectStoreExt;
        self.store
            .get(&object_store::path::Path::from(key))
            .await
            .unwrap_or_else(|e| panic!("shard object {key} must exist: {e}"))
            .bytes()
            .await
            .unwrap()
            .to_vec()
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
    let (revisions,): (i64,) = sqlx::query_as("SELECT count(*) FROM shards")
        .fetch_one(&h.pool().await)
        .await
        .unwrap();
    assert_eq!(revisions, 1, "one commit, one Shard revision");
}

#[tokio::test]
async fn a_new_commit_publishes_a_new_revision_and_never_mutates_the_old_shard() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let first = h.shard_status().await;

    // Pin the first Shard's bytes before the repo moves on.
    let manifest_key = h.manifest_key().await;
    let graph_key = h.graph_key().await;
    let first_manifest = h.object_bytes(&manifest_key).await;
    let first_graph = h.object_bytes(&graph_key).await;

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
        .fetch_one(&h.pool().await)
        .await
        .unwrap();
    assert_eq!(revisions, 2, "every published revision stays recorded");
    assert_eq!(
        h.object_bytes(&manifest_key).await,
        first_manifest,
        "an existing Shard's manifest must never change"
    );
    assert_eq!(
        h.object_bytes(&graph_key).await,
        first_graph,
        "an existing Shard's graph segment must never change"
    );
}

#[tokio::test]
async fn the_manifest_records_commit_checksums_counts_and_schema_version() {
    use sha2::Digest;

    let h = Harness::boot().await;
    let head = git(&h.repo_dir, &["rev-parse", "HEAD"]);
    h.add_repo().await;
    h.sync_and_index().await;

    let shard = h.shard_status().await;
    let manifest_bytes = h.object_bytes(&h.manifest_key().await).await;
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
    let graph_bytes = h.object_bytes(&h.graph_key().await).await;
    assert_eq!(
        segment["sha256"].as_str(),
        Some(hex::encode(sha2::Sha256::digest(&graph_bytes)).as_str()),
        "the manifest checksum must match the stored graph segment"
    );
    assert_eq!(segment["bytes"], graph_bytes.len() as u64);
}

#[tokio::test]
async fn every_edge_row_carries_provenance_and_confidence() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let graph_bytes = h.object_bytes(&h.graph_key().await).await;

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

#[tokio::test]
async fn a_failing_index_job_surfaces_its_error_backs_off_and_recovers() {
    let h = Harness::boot().await;
    h.add_repo().await;
    assert!(h.sync.run_once().await.unwrap(), "fetch job must run");

    // The worst case: the cache mirror is evicted AND the forge is
    // unreachable, so the worker can neither use nor refetch the mirror.
    let mirror = std::fs::read_dir(&h.cache)
        .unwrap()
        .next()
        .expect("the fetch must have left a mirror")
        .unwrap()
        .path();
    std::fs::remove_dir_all(&mirror).unwrap();
    let hidden_origin = h.repo_dir.with_file_name("gadgets-offline");
    std::fs::rename(&h.repo_dir, &hidden_origin).unwrap();

    assert!(
        h.indexer
            .run_once()
            .await
            .expect("a failed index run is handled, not an error"),
        "the job must still be claimed"
    );

    let body = admin_status_body(&h.base).await;
    let index = &body["repos"][0]["index"];
    assert_eq!(index["state"], "retrying", "got: {body}");
    assert_eq!(index["attempts"], 1);
    assert!(
        index["last_error"].as_str().is_some_and(|e| !e.is_empty()),
        "the error must say what failed, got: {body}"
    );
    assert_eq!(
        body["repos"][0]["shard"],
        serde_json::Value::Null,
        "no Shard may be published for a failed index"
    );

    assert!(
        !h.indexer.run_once().await.unwrap(),
        "a failed job must not be due again immediately (backoff)"
    );

    // The cause clears (the forge is reachable again); once the backoff
    // elapses, indexing converges on its own by refetching the mirror.
    std::fs::rename(&hidden_origin, &h.repo_dir).unwrap();
    sqlx::query("UPDATE jobs SET run_after = now() WHERE kind = 'index'")
        .execute(&h.pool().await)
        .await
        .unwrap();
    assert!(
        h.indexer.run_once().await.unwrap(),
        "due again after backoff"
    );

    let body = admin_status_body(&h.base).await;
    assert_eq!(body["repos"][0]["index"]["state"], "indexed", "got: {body}");
    assert!(
        body["repos"][0]["shard"]["revision"].as_str().is_some(),
        "recovery must publish the Shard, got: {body}"
    );
}

#[tokio::test]
async fn re_indexing_a_published_commit_completes_without_a_checkout() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    // The git cache evaporates (cache eviction, fresh worker host) and a
    // new index job arrives for the same, already-published commit. The
    // published Shard answers it — no checkout required.
    std::fs::remove_dir_all(&h.cache).unwrap();
    sqlx::query("INSERT INTO jobs (kind, repo_id) SELECT 'index', id FROM repos")
        .execute(&h.pool().await)
        .await
        .unwrap();

    assert!(
        h.indexer.run_once().await.unwrap(),
        "the re-queued job must be claimed"
    );
    let body = admin_status_body(&h.base).await;
    assert_eq!(
        body["repos"][0]["index"]["state"], "indexed",
        "an already-published revision must complete, not retry, got: {body}"
    );
    assert!(
        body["repos"][0]["shard"]["revision"].as_str().is_some(),
        "got: {body}"
    );
}

#[tokio::test]
async fn an_index_worker_without_the_mirror_fetches_it_itself() {
    let h = Harness::boot().await;
    h.add_repo().await;
    assert!(h.sync.run_once().await.unwrap(), "fetch job must run");

    // The index job lands on a different worker host: same control plane
    // and object storage, but its own — empty — git cache.
    let other_host_cache = tempfile::tempdir().unwrap();
    let lone_indexer = yg_index::IndexWorker::new(
        control_plane(&h.db_name).await,
        h.store.clone(),
        other_host_cache.path(),
    );
    assert!(
        lone_indexer
            .run_once()
            .await
            .expect("indexing must not error"),
        "the index job must be claimed"
    );

    let body = admin_status_body(&h.base).await;
    assert_eq!(
        body["repos"][0]["index"]["state"], "indexed",
        "a worker without the mirror must fetch it and index, got: {body}"
    );
    assert_eq!(body["repos"][0]["shard"]["nodes"], 4, "got: {body}");
}

#[tokio::test]
async fn a_commit_fetched_mid_index_still_gets_indexed() {
    let h = Harness::boot().await;
    h.add_repo().await;
    assert!(h.sync.run_once().await.unwrap(), "fetch job must run");

    // A worker claims the index job for the first commit and is slow.
    let control = control_plane(&h.db_name).await;
    let job = control
        .claim_due_index(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("the queued index job must be claimable");

    // While it grinds, the repo moves on and a fresh fetch lands.
    std::fs::write(
        h.repo_dir.join("extra.go"),
        "package main\n\nfunc Extra() {}\n",
    )
    .unwrap();
    git(&h.repo_dir, &["add", "."]);
    git(&h.repo_dir, &["commit", "-m", "extra"]);
    let new_head = git(&h.repo_dir, &["rev-parse", "HEAD"]);
    h.add_repo().await;
    assert!(h.sync.run_once().await.unwrap(), "second fetch must run");

    // The slow worker finishes its (now stale) run and reports it.
    let applied = control
        .complete_index(
            &job,
            yg_control::ShardRecord {
                revision: &format!("{}-syntactic-v1", job.commit),
                manifest_key: &format!("shards/{}/test/manifest.json", job.repo_id),
                commit_sha: &job.commit,
                provenance_level: "syntactic",
                node_count: 4,
                edge_count: 2,
            },
        )
        .await
        .unwrap();
    assert!(applied, "the live lease holder's completion must apply");

    // The newer commit must not be lost: indexing converges on it.
    assert!(
        h.indexer.run_once().await.unwrap(),
        "a fetch that landed mid-index must leave index work pending"
    );
    let shard = h.shard_status().await;
    assert!(
        shard["revision"]
            .as_str()
            .is_some_and(|r| r.starts_with(&new_head)),
        "the current Shard must cover the newest synced commit, got: {shard}"
    );
}

/// multi_thread: the yg binary blocks this thread while the in-process
/// server it queries runs on the same test runtime.
#[tokio::test(flavor = "multi_thread")]
async fn yg_admin_status_shows_the_shard_revision_and_counts() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let revision = h.shard_status().await["revision"]
        .as_str()
        .expect("indexed repo has a revision")
        .to_string();

    let status = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &h.base)
        .env("YG_TOKEN", "ygt_test_token")
        .args(["admin", "status"])
        .assert()
        .success();
    let stdout = String::from_utf8(status.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(&revision[..12]),
        "the human report must show the Shard revision, got:\n{stdout}"
    );
    assert!(
        stdout.contains("4 nodes") && stdout.contains("2 edges"),
        "the human report must show the graph counts, got:\n{stdout}"
    );
}

/// multi_thread: the yg binary blocks this thread while the in-process
/// HTTP client polls it from the same test runtime.
#[tokio::test(flavor = "multi_thread")]
async fn yg_serve_role_all_indexes_an_added_repo_end_to_end() {
    use std::io::BufRead;

    let (fixture, _repo_dir, fixture_url) = go_fixture_repo();
    let db_name = create_test_db().await;
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("yg"))
        .env("YG_LISTEN", "127.0.0.1:0")
        .env("YG_DATABASE_URL", format!("{DEV_POSTGRES}/{db_name}"))
        .env("YG_BOOTSTRAP_TOKEN", "ygt_test_token")
        .env("YG_GIT_CACHE", fixture.path().join("git-cache"))
        .args(["serve", "--role=all"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut server = KillOnDrop(child);
    let stdout = server.0.stdout.take().unwrap();
    let first_line = std::io::BufReader::new(stdout)
        .lines()
        .next()
        .expect("yg serve must announce its address")
        .unwrap();
    let url = first_line
        .strip_prefix("listening on ")
        .unwrap()
        .to_string();

    post_repo(&url, serde_json::json!({"url": fixture_url})).await;

    // The serve process syncs *and indexes* on its own — the issue's
    // demo flow: add a Go repo, watch its Shard appear.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let body = admin_status_body(&url).await;
        let shard = &body["repos"][0]["shard"];
        if shard["revision"].as_str().is_some() {
            assert_eq!(shard["nodes"], 4, "got: {body}");
            assert_eq!(shard["edges"], 2, "got: {body}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "repo never got indexed; last status: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
