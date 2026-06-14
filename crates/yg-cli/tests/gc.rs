//! Shard garbage collection (issue #9): a superseded Shard's objects are
//! reclaimed only after a grace window, and the current Shard is never
//! touched. Runs against the dev compose stack like the other e2e
//! targets (see docs/DEVELOPMENT.md).

mod common;

use common::*;
use std::time::Duration;

impl Harness {
    /// A pool on the harness database, for SQL-level assertions.
    async fn pool(&self) -> sqlx::PgPool {
        sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", self.db_name))
            .await
            .unwrap()
    }

    /// The repo's id (these tests register exactly one repo).
    async fn repo_id(&self) -> i64 {
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM repos")
            .fetch_one(&self.pool().await)
            .await
            .unwrap();
        id
    }

    /// How many Shard rows the control plane records.
    async fn shard_row_count(&self) -> i64 {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM shards")
            .fetch_one(&self.pool().await)
            .await
            .unwrap();
        n
    }

    /// The repo's current-Shard pointer (these tests register one repo).
    async fn current_shard_id(&self) -> i64 {
        let (id,): (Option<i64>,) = sqlx::query_as("SELECT current_shard_id FROM repos")
            .fetch_one(&self.pool().await)
            .await
            .unwrap();
        id.expect("the repo must have a current Shard")
    }

    /// The sole Shard's `published_at` as a unix epoch (single-shard tests).
    async fn published_epoch(&self) -> f64 {
        let (epoch,): (f64,) =
            sqlx::query_as("SELECT extract(epoch FROM published_at)::float8 FROM shards")
                .fetch_one(&self.pool().await)
                .await
                .unwrap();
        epoch
    }

    /// Whether an object exists in the Shard store.
    async fn object_present(&self, key: &str) -> bool {
        use object_store::ObjectStoreExt;
        match self.store.get(&object_store::path::Path::from(key)).await {
            Ok(_) => true,
            Err(object_store::Error::NotFound { .. }) => false,
            Err(e) => panic!("unexpected object store error for {key}: {e}"),
        }
    }

    /// Commit `files` onto the fixture's default branch.
    fn push_commit(&self, message: &str, files: &[(&str, &str)]) {
        for (path, contents) in files {
            std::fs::write(self.repo_dir.join(path), contents).unwrap();
        }
        git(&self.repo_dir, &["add", "."]);
        git(&self.repo_dir, &["commit", "-m", message]);
    }
}

#[tokio::test]
async fn a_superseded_shard_is_reclaimed_only_after_the_grace_window() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    // Pin the first Shard's identity before the repo moves on.
    let first_commit = git(&h.repo_dir, &["rev-parse", "HEAD"]);
    let repo_id = h.repo_id().await;
    let first_revision = yg_shard::syntactic_revision(&first_commit);
    let first_manifest = yg_shard::manifest_key(repo_id, &first_revision);
    let first_graph = yg_shard::graph_segment_key(repo_id, &first_revision);
    assert!(
        h.object_present(&first_manifest).await,
        "first Shard is stored"
    );

    // A push supersedes the first Shard with a second revision (re-add
    // queues the fresh fetch the poll loop would otherwise trigger).
    h.push_commit(
        "add Greet",
        &[("extra.go", "package main\n\nfunc Greet() {}\n")],
    );
    h.add_repo().await;
    h.sync_and_index().await;
    assert_eq!(h.shard_row_count().await, 2, "both revisions are recorded");

    // Within the grace window nothing is reclaimed — an in-flight query
    // that resolved the old pointer must still find its Shard.
    assert_eq!(
        h.indexer.gc_once(Duration::from_secs(3600)).await.unwrap(),
        0,
        "a Shard superseded seconds ago is still inside the grace window"
    );
    assert!(
        h.object_present(&first_manifest).await,
        "the superseded Shard's objects survive inside the grace window"
    );
    assert_eq!(h.shard_row_count().await, 2, "and its row survives");

    // Past the grace window the superseded Shard is reclaimed — objects
    // and row both gone.
    assert_eq!(
        h.indexer.gc_once(Duration::ZERO).await.unwrap(),
        1,
        "past the grace window the one superseded Shard is collected"
    );
    assert!(
        !h.object_present(&first_manifest).await,
        "the superseded Shard's manifest is deleted"
    );
    assert!(
        !h.object_present(&first_graph).await,
        "and its graph segment is deleted"
    );
    assert_eq!(
        h.shard_row_count().await,
        1,
        "only the current Shard's row remains"
    );

    // The current Shard is untouched, and the repo still answers queries.
    let second_commit = git(&h.repo_dir, &["rev-parse", "HEAD"]);
    let second_manifest =
        yg_shard::manifest_key(repo_id, &yg_shard::syntactic_revision(&second_commit));
    assert!(
        h.object_present(&second_manifest).await,
        "the current Shard's objects are untouched"
    );
    let body = h
        .verb_ok("search", serde_json::json!({"query": "Greet"}))
        .await;
    assert!(
        body["hits"]
            .as_array()
            .is_some_and(|hits| hits.iter().any(|hit| hit["name"] == "Greet")),
        "the current Shard still serves queries after GC, got: {body}"
    );
}

#[tokio::test]
async fn the_current_shard_is_never_collected() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let repo_id = h.repo_id().await;
    let commit = git(&h.repo_dir, &["rev-parse", "HEAD"]);
    let manifest = yg_shard::manifest_key(repo_id, &yg_shard::syntactic_revision(&commit));

    // Even with a zero grace window, the sole — current — Shard is not
    // superseded, so nothing is reclaimed.
    assert_eq!(
        h.indexer.gc_once(Duration::ZERO).await.unwrap(),
        0,
        "the current Shard is never eligible for GC"
    );
    assert!(
        h.object_present(&manifest).await,
        "the current Shard's objects are left in place"
    );
    assert_eq!(h.shard_row_count().await, 1);
}

#[tokio::test]
async fn the_gc_guard_refuses_to_delete_a_current_shard() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    // Directly attempt to reclaim the current Shard's row, as the sweep
    // would if a force-push made this row current between its eligibility
    // scan and the delete. The guard must refuse and leave the row.
    let current = h.current_shard_id().await;
    let control = control_plane(&h.db_name).await;
    let deleted = control.delete_superseded_shard(current).await.unwrap();
    assert!(
        !deleted,
        "the guard must not delete a Shard a repo points at"
    );
    assert_eq!(
        h.shard_row_count().await,
        1,
        "the current Shard's row survives"
    );
}

#[tokio::test]
async fn a_shard_that_becomes_current_again_is_not_collected() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    // Pin the first Shard (the one that will be superseded then resurrected).
    let first_commit = git(&h.repo_dir, &["rev-parse", "HEAD"]);
    let repo_id = h.repo_id().await;
    let first_manifest =
        yg_shard::manifest_key(repo_id, &yg_shard::syntactic_revision(&first_commit));

    // Move forward to a second revision, superseding the first.
    h.push_commit(
        "add Greet",
        &[("extra.go", "package main\n\nfunc Greet() {}\n")],
    );
    h.add_repo().await;
    h.sync_and_index().await;
    assert_eq!(h.shard_row_count().await, 2);

    // Force-push back to the first commit. Revisions are deterministic, so
    // re-indexing it republishes the first revision — reusing its row and
    // making it current again, while the second revision is now superseded.
    git(&h.repo_dir, &["reset", "--hard", &first_commit]);
    h.add_repo().await;
    h.sync_and_index().await;
    assert_eq!(
        h.current_shard_id().await,
        // The reused first-revision row is current again.
        {
            let (id,): (i64,) = sqlx::query_as("SELECT id FROM shards WHERE commit_sha = $1")
                .bind(&first_commit)
                .fetch_one(&h.pool().await)
                .await
                .unwrap();
            id
        },
        "the resurrected first revision is current again"
    );

    // GC with zero grace: the now-current first revision must survive; only
    // the (now superseded) second revision is collected.
    assert_eq!(h.indexer.gc_once(Duration::ZERO).await.unwrap(), 1);
    assert!(
        h.object_present(&first_manifest).await,
        "the resurrected Shard's objects must not be deleted — it is current"
    );
    assert_eq!(
        h.shard_row_count().await,
        1,
        "only the superseded revision was collected"
    );
}

#[tokio::test]
async fn a_published_but_never_current_shard_is_reclaimed_via_its_publish_time() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo_id = h.repo_id().await;

    // A Shard that was published but never became current (a superseded
    // index result that lost the swap race) carries no superseded_at; GC
    // must anchor its grace on published_at instead. Stand one in, aged a
    // day, never pointed at.
    sqlx::query(
        "INSERT INTO shards (repo_id, revision, manifest_key, commit_sha,
                             provenance_level, node_count, edge_count,
                             published_at, superseded_at)
         VALUES ($1, 'never-current-rev', 'shards/x/never-current-rev/manifest.json',
                 'feedfacefeedfacefeedfacefeedfacefeedface', 'syntactic', 0, 0,
                 now() - interval '1 day', NULL)",
    )
    .bind(repo_id)
    .execute(&h.pool().await)
    .await
    .unwrap();
    assert_eq!(h.shard_row_count().await, 2);

    // Past grace by published_at, not pointed at — reclaimed; the current
    // Shard (no superseded_at, pointed at) is left.
    assert_eq!(h.indexer.gc_once(Duration::ZERO).await.unwrap(), 1);
    assert_eq!(
        h.shard_row_count().await,
        1,
        "the published-but-never-current Shard is reclaimed via published_at"
    );
}

#[tokio::test]
async fn republishing_a_revision_refreshes_its_grace_window() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let first = h.published_epoch().await;

    // Re-index the same commit: the deterministic revision reuses its row.
    // published_at must move to now() so a resurrected revision left
    // non-current gets a fresh grace window, not its stale original one.
    h.add_repo().await;
    h.sync_and_index().await;

    assert_eq!(
        h.shard_row_count().await,
        1,
        "the same revision reuses its row, not a second one"
    );
    let second = h.published_epoch().await;
    assert!(
        second > first,
        "republishing a revision must restart its grace window, got {first} then {second}"
    );
}
