//! Job queue hygiene and control-plane schema constraints (issue #49):
//! terminal jobs age out on the GC cadence, the vocabulary columns
//! reject unknown values at the database, the claim and GC plans are
//! index-served, and the qualifier grammar is single-sourced. Runs
//! against the dev compose stack like the other e2e targets.

mod common;

use common::*;
use std::time::Duration;

use yg_control::{JobKind, REPO_QUALIFIER_SQL, SUPERSEDED_SHARDS_PAST_GRACE_SQL, claim_due_sql};

/// A pool on a test database, for SQL-level assertions.
async fn pool(db_name: &str) -> sqlx::PgPool {
    sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap()
}

/// A migrated test database with one forge and one repo row, for tests
/// that exercise the schema without booting a server.
async fn schema_fixture() -> (String, sqlx::PgPool, i64) {
    let db_name = create_test_db().await;
    control_plane(&db_name).await; // migrates
    let pool = pool(&db_name).await;
    let (forge_id,): (i64,) =
        sqlx::query_as("INSERT INTO forges (kind, base_url) VALUES ('git', $1) RETURNING id")
            .bind("https://forge.example.test")
            .fetch_one(&pool)
            .await
            .unwrap();
    let (repo_id,): (i64,) = sqlx::query_as(
        "INSERT INTO repos (forge_id, slug, qualifier) VALUES ($1, 'acme/widgets', $2) RETURNING id",
    )
    .bind(forge_id)
    .bind("forge.example.test/acme/widgets")
    .fetch_one(&pool)
    .await
    .unwrap();
    (db_name, pool, repo_id)
}

#[tokio::test]
async fn terminal_jobs_older_than_the_retention_window_are_removed() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let pool = pool(&h.db_name).await;

    // The pipeline settled a fetch and an index job; both are terminal
    // and stamped with when they finished.
    let (done, stamped): (i64, i64) =
        sqlx::query_as("SELECT count(*), count(finished_at) FROM jobs WHERE state = 'done'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(done, 2, "fetch and index both settled terminal");
    assert_eq!(stamped, done, "every terminal job is stamped finished_at");

    // Inside the retention window nothing is removed — the rows are
    // still `yg admin status` history.
    assert_eq!(
        h.indexer
            .retire_terminal_jobs(Duration::from_secs(3600))
            .await
            .unwrap(),
        0,
        "jobs finished seconds ago are inside the retention window"
    );

    // A queued job must survive any retention pass — it is work, not
    // history, and carries no finished_at.
    sqlx::query("INSERT INTO jobs (kind, repo_id, state) VALUES ('fetch', $1, 'queued')")
        .bind(h.repo_id().await)
        .execute(&pool)
        .await
        .unwrap();

    // Age the terminal rows past the window: the GC cadence removes
    // exactly them.
    sqlx::query("UPDATE jobs SET finished_at = now() - interval '2 hours' WHERE state = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        h.indexer
            .retire_terminal_jobs(Duration::from_secs(3600))
            .await
            .unwrap(),
        2,
        "terminal jobs past the retention window are removed"
    );
    let (kept,): (i64,) = sqlx::query_as("SELECT count(*) FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 1, "only the queued (non-terminal) job survives");
}

impl Harness {
    /// The repo's id (this file's harness tests register exactly one).
    async fn repo_id(&self) -> i64 {
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM repos")
            .fetch_one(&pool(&self.db_name).await)
            .await
            .unwrap();
        id
    }
}

#[tokio::test]
async fn unknown_kind_state_and_provenance_values_fail_at_the_database() {
    let (_db, pool, repo_id) = schema_fixture().await;

    // Every Rust job kind inserts cleanly…
    for kind in JobKind::ALL {
        sqlx::query(
            "INSERT INTO jobs (kind, repo_id, state, finished_at) VALUES ($1, $2, 'done', now())",
        )
        .bind(kind.as_str())
        .bind(repo_id)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("kind {:?} must satisfy the constraint: {e}", kind.as_str()));
    }
    // …and a kind outside the vocabulary fails loudly instead of
    // sitting invisible and unclaimable forever.
    let violation = |err: sqlx::Error, constraint: &str| {
        let db_err = match err {
            sqlx::Error::Database(e) => e,
            other => panic!("expected a database error, got {other:?}"),
        };
        assert_eq!(db_err.constraint(), Some(constraint), "{db_err}");
    };
    violation(
        sqlx::query("INSERT INTO jobs (kind, repo_id) VALUES ('embed', $1)")
            .bind(repo_id)
            .execute(&pool)
            .await
            .unwrap_err(),
        "jobs_kind_check",
    );
    violation(
        sqlx::query("INSERT INTO jobs (kind, repo_id, state) VALUES ('fetch', $1, 'zombie')")
            .bind(repo_id)
            .execute(&pool)
            .await
            .unwrap_err(),
        "jobs_state_check",
    );

    // Same for the Shard provenance vocabulary.
    for provenance in yg_shard::Provenance::ALL {
        sqlx::query(
            "INSERT INTO shards (repo_id, revision, manifest_key, commit_sha,
                                 provenance_level, node_count, edge_count)
             VALUES ($1, $2, 'shards/x/manifest.json',
                     'feedfacefeedfacefeedfacefeedfacefeedface', $3, 0, 0)",
        )
        .bind(repo_id)
        .bind(format!("rev-{}", provenance.as_str()))
        .bind(provenance.as_str())
        .execute(&pool)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "provenance {:?} must satisfy the constraint: {e}",
                provenance.as_str()
            )
        });
    }
    violation(
        sqlx::query(
            "INSERT INTO shards (repo_id, revision, manifest_key, commit_sha,
                                 provenance_level, node_count, edge_count)
             VALUES ($1, 'rev-unknown', 'shards/x/manifest.json',
                     'feedfacefeedfacefeedfacefeedfacefeedface', 'vibes', 0, 0)",
        )
        .bind(repo_id)
        .execute(&pool)
        .await
        .unwrap_err(),
        "shards_provenance_level_check",
    );
}

/// Like yg-control's kind-constraint unit test, but for the provenance
/// vocabulary: yg-control cannot see [`yg_shard::Provenance`], so the
/// clause the migration installs is pinned here, where both crates are
/// in view. Needs no database — it reads the migration source.
#[test]
fn the_provenance_constraint_mirrors_the_rust_vocabulary() {
    let migration = include_str!("../../yg-control/migrations/0011_job_queue_hygiene.sql");
    let clause = format!(
        "provenance_level IN ({})",
        yg_shard::Provenance::ALL
            .map(|level| format!("'{}'", level.as_str()))
            .join(", ")
    );
    assert!(
        migration.contains(&clause),
        "migration 0011 must constrain provenance_level to {clause}"
    );
}

/// The plan for `sql`, with sequential scans priced out so the plan
/// shows whether an index CAN serve the query — on test-sized tables
/// the planner would otherwise prefer a seq scan regardless of what
/// indexes exist, which is exactly the signal these tests need to
/// suppress.
async fn plan_without_seqscan(pool: &sqlx::PgPool, sql: &str) -> String {
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .unwrap();
    let rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    rows.into_iter()
        .map(|(line,)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_claim_and_gc_queries_are_served_by_indexes() {
    let (_db, pool, _repo_id) = schema_fixture().await;

    // The GC sweep's pointer anti-join probes repos per shard row; the
    // partial index on the current-Shard pointer must serve it.
    let gc_sql = SUPERSEDED_SHARDS_PAST_GRACE_SQL.replace("$1", "3600");
    let plan = plan_without_seqscan(&pool, &gc_sql).await;
    assert!(
        plan.contains("repos_current_shard"),
        "the GC anti-join must probe the pointer index, got:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan on repos"),
        "the GC anti-join must not sequential-scan repos, got:\n{plan}"
    );

    // The claim query orders by run_after alone (priority is gone), so
    // the partial indexes over non-terminal jobs can serve it. Which
    // one the planner probes on a test-sized table is its choice; what
    // must hold is that none of the claim's scans needs a seq scan.
    for kind in JobKind::ALL {
        let claim = claim_due_sql(kind, "", "r.slug").replace("$1", "60");
        let plan = plan_without_seqscan(&pool, &claim).await;
        assert!(
            !plan.contains("Seq Scan"),
            "the {:?} claim must be index-served throughout, got:\n{plan}",
            kind.as_str()
        );
    }
}

/// The other direction of the single-sourced qualifier grammar (the
/// yg-control unit tests pin the migration to `REPO_QUALIFIER_SQL`
/// verbatim): the SQL rendering computes exactly what `repo_qualifier`
/// computes, over forge roots that exercise every branch of both.
#[tokio::test]
async fn the_sql_qualifier_rendering_matches_the_rust_grammar() {
    let (_db, pool, _repo_id) = schema_fixture().await;
    let corpus = [
        "https://github.com",
        "https://github.com/",
        "http://git.corp.example:8443",
        "file:///tmp/fixtures",
        "file:///",
        "git.example.com",
        "git.example.com/",
        "HTTPS://Mixed.Case",
        "://no-scheme-root",
        "a://b://c",
        "",
    ];
    for base_url in corpus {
        let (from_sql,): (String,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {REPO_QUALIFIER_SQL}
             FROM (SELECT $1::text AS base_url) f, (SELECT $2::text AS slug) r"
        )))
        .bind(base_url)
        .bind("acme/widgets")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            from_sql,
            yg_control::repo_qualifier(base_url, "acme/widgets"),
            "SQL and Rust qualifier grammars diverge on {base_url:?}"
        );
    }
}
