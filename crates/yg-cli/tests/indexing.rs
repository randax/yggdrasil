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

#[tokio::test]
async fn indexing_a_synced_go_repo_publishes_a_shard_with_symbols_and_counts() {
    let (fixture, _repo_dir, fixture_url) = go_fixture_repo();
    let cache = fixture.path().join("git-cache");

    let db_name = create_test_db().await;
    let config = test_config(&db_name);
    let store = config
        .object_store
        .connect()
        .expect("dev MinIO must be reachable");
    let server = serve(config).await.expect("boot");
    let base = format!("http://{}", server.local_addr());

    post_repo(&base, serde_json::json!({"url": fixture_url})).await;

    // Sync produces the checkout; indexing consumes it.
    let sync = yg_sync::SyncWorker::new(control_plane(&db_name).await, &cache);
    assert!(sync.run_once().await.unwrap(), "fetch job must run");
    let indexer = yg_index::IndexWorker::new(control_plane(&db_name).await, store, &cache);
    assert!(
        indexer.run_once().await.expect("indexing must not error"),
        "a successful fetch must queue an index job"
    );

    let body = admin_status_body(&base).await;
    let repo = &body["repos"][0];
    let shard = &repo["shard"];
    assert!(
        shard["revision"].as_str().is_some_and(|r| !r.is_empty()),
        "an indexed repo must show its Shard revision, got: {body}"
    );
    // main.go and README.md are File nodes; Hello and main are Symbols,
    // each defined by main.go.
    assert_eq!(shard["nodes"], 4, "got: {body}");
    assert_eq!(shard["edges"], 2, "got: {body}");

    assert!(
        !indexer.run_once().await.unwrap(),
        "the index queue must be drained after the one job"
    );
}
