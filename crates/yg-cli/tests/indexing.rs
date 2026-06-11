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
        let indexer = yg_index::IndexWorker::new(control_plane(&db_name).await, store, &cache);
        Self {
            fixture,
            repo_dir,
            fixture_url,
            db_name,
            base,
            _server: server,
            sync,
            indexer,
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
