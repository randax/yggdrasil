//! Postgres models, job queue.

use anyhow::Context;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Where the control plane lives when `YG_DATABASE_URL` says nothing:
/// the in-repo dev compose stack.
pub const DEFAULT_DATABASE_URL: &str = "postgres://yggdrasil:yggdrasil@localhost:5432/yggdrasil";

/// Handle to the control-plane database. The single entry point for
/// everything the Index Server keeps in Postgres. Clones share one pool.
#[derive(Clone)]
pub struct ControlPlane {
    pool: PgPool,
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A repository registration, as `yg admin repo add` hands it over.
pub struct AddRepo<'a> {
    /// Forge kind: `github`, or `git` for any other remote.
    pub forge_kind: &'a str,
    /// Forge root, unique per forge, e.g. `https://github.com`.
    pub base_url: &'a str,
    /// Env var the Forge token is read from, if this forge has one.
    pub token_env: Option<&'a str>,
    /// Repo path on the forge, e.g. `acme/widgets`.
    pub slug: &'a str,
    /// Shallow-clone override; `None` fetches full history (the default).
    pub fetch_depth: Option<i32>,
}

pub struct AddRepoOutcome {
    pub repo_id: i64,
    /// False when the repo was already registered (idempotent re-add).
    pub created: bool,
    /// False when a fetch was already in flight, so this add queued
    /// nothing new.
    pub fetch_queued: bool,
}

/// A fetch job a worker holds the lease on, with everything needed to
/// run it: where to clone from and how.
#[derive(sqlx::FromRow)]
pub struct LeasedFetch {
    pub job_id: i64,
    pub repo_id: i64,
    /// Failures so far (0 on the first run).
    pub attempts: i32,
    pub slug: String,
    /// Shallow-clone override; `None` = full history.
    pub fetch_depth: Option<i32>,
    /// Forge root; the clone URL is `{base_url}/{slug}`.
    pub base_url: String,
    /// Env var holding the Forge token, if the forge has one.
    pub token_env: Option<String>,
    /// Opaque fencing token for this claim. `complete_fetch`/`fail_fetch`
    /// only apply while it still matches — a worker that outlived its
    /// lease (the job was re-claimed) has its result discarded.
    lease_token: String,
}

/// An index job a worker holds the lease on: which repo to index, the
/// commit its sync position points at, and where to clone from — an
/// indexing worker whose local cache lacks the mirror fetches it itself.
#[derive(sqlx::FromRow)]
pub struct LeasedIndex {
    pub job_id: i64,
    pub repo_id: i64,
    /// Failures so far (0 on the first run).
    pub attempts: i32,
    pub slug: String,
    /// The commit to index — the repo's sync position at claim time.
    pub commit: String,
    /// Forge root; the clone URL is `{base_url}/{slug}`.
    pub base_url: String,
    /// Env var holding the Forge token, if the forge has one.
    pub token_env: Option<String>,
    /// Shallow-clone override; `None` = full history.
    pub fetch_depth: Option<i32>,
    /// Opaque fencing token; see [`LeasedFetch::lease_token`].
    lease_token: String,
}

/// A published Shard as the control plane records it: the row inserted
/// into `shards` when an index job completes.
pub struct ShardRecord<'a> {
    pub revision: &'a str,
    pub manifest_key: &'a str,
    pub commit_sha: &'a str,
    /// `syntactic` | `precise` (ADR 0002).
    pub provenance_level: &'a str,
    pub node_count: i64,
    pub edge_count: i64,
}

/// One repo's row in `yg admin status`.
#[derive(sqlx::FromRow)]
pub struct RepoSyncStatus {
    pub slug: String,
    /// The forge's base URL.
    pub forge: String,
    pub last_synced_commit: Option<String>,
    /// State of the in-flight fetch job (`queued` | `leased`), if any.
    pub job_state: Option<String>,
    pub attempts: i32,
    pub last_error: Option<String>,
    /// State of the in-flight index job (`queued` | `leased`), if any.
    pub index_job_state: Option<String>,
    pub index_attempts: i32,
    pub index_last_error: Option<String>,
    /// The repo's current Shard, if it has ever been indexed.
    pub shard_revision: Option<String>,
    pub shard_node_count: Option<i64>,
    pub shard_edge_count: Option<i64>,
}

impl ControlPlane {
    /// Connect and bring the schema up to date. Applied migrations are
    /// tracked in `_sqlx_migrations`, so restarting against an
    /// already-migrated database is a no-op.
    pub async fn connect_and_migrate(database_url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .context("connecting to control-plane Postgres")?;
        MIGRATOR
            .run(&pool)
            .await
            .context("running control-plane migrations")?;
        Ok(Self { pool })
    }

    /// Register a repository for Sync: upsert its Forge, the repo row,
    /// and an exact-slug include rule, and queue a fetch job unless one
    /// is already in flight. Idempotent — re-adding an existing repo
    /// changes nothing but its depth override (and re-queues a fetch if
    /// none is pending).
    pub async fn add_repo(&self, repo: AddRepo<'_>) -> anyhow::Result<AddRepoOutcome> {
        let mut tx = self.pool.begin().await?;
        // DO UPDATE (rather than DO NOTHING) so RETURNING yields the id
        // on conflict too. Overwriting kind is safe: it's derived from
        // the URL host, so every add of this base_url carries the same
        // value. token_env only backfills a missing value — an explicit
        // per-forge override is never clobbered by a re-add.
        let (forge_id,): (i64,) = sqlx::query_as(
            "INSERT INTO forges (kind, base_url, token_env) VALUES ($1, $2, $3)
             ON CONFLICT (base_url) DO UPDATE
             SET kind = excluded.kind,
                 token_env = coalesce(forges.token_env, excluded.token_env)
             RETURNING id",
        )
        .bind(repo.forge_kind)
        .bind(repo.base_url)
        .bind(repo.token_env)
        .fetch_one(&mut *tx)
        .await?;
        // (xmax = 0) distinguishes a fresh insert from an upsert of an
        // existing row.
        let (repo_id, created): (i64, bool) = sqlx::query_as(
            "INSERT INTO repos (forge_id, slug, fetch_depth) VALUES ($1, $2, $3)
             ON CONFLICT (forge_id, slug) DO UPDATE SET fetch_depth = excluded.fetch_depth
             RETURNING id, (xmax = 0)",
        )
        .bind(forge_id)
        .bind(repo.slug)
        .bind(repo.fetch_depth)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO rules (forge_id, pattern, action) VALUES ($1, $2, 'include')
             ON CONFLICT (forge_id, pattern, action) DO NOTHING",
        )
        .bind(forge_id)
        .bind(repo.slug)
        .execute(&mut *tx)
        .await?;
        let fetch_queued = enqueue_job_unless_in_flight(&mut tx, "fetch", repo_id).await?;
        tx.commit().await?;
        Ok(AddRepoOutcome {
            repo_id,
            created,
            fetch_queued,
        })
    }

    /// Every registered repo with its sync position: last synced commit,
    /// the in-flight fetch job, if any, and its current Shard, if any.
    pub async fn admin_status(&self) -> anyhow::Result<Vec<RepoSyncStatus>> {
        let rows = sqlx::query_as(
            // Plain joins suffice: jobs_one_in_flight_per_repo_kind
            // guarantees at most one non-done job per (repo, kind).
            "SELECT r.slug, f.base_url AS forge, r.last_synced_commit,
                    j.state AS job_state, coalesce(j.attempts, 0) AS attempts,
                    j.last_error,
                    i.state AS index_job_state,
                    coalesce(i.attempts, 0) AS index_attempts,
                    i.last_error AS index_last_error,
                    s.revision AS shard_revision,
                    s.node_count AS shard_node_count,
                    s.edge_count AS shard_edge_count
             FROM repos r
             JOIN forges f ON f.id = r.forge_id
             LEFT JOIN shards s ON s.id = r.current_shard_id
             LEFT JOIN jobs j
                 ON j.repo_id = r.id AND j.kind = 'fetch' AND j.state <> 'done'
             LEFT JOIN jobs i
                 ON i.repo_id = r.id AND i.kind = 'index' AND i.state <> 'done'
             ORDER BY r.slug",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Claim the next due fetch job under a lease. `FOR UPDATE SKIP
    /// LOCKED` lets parallel workers claim without contention; a job whose
    /// lease expired (worker crashed mid-fetch) is due again. Returns
    /// `None` when nothing is due.
    pub async fn claim_due_fetch(
        &self,
        lease: std::time::Duration,
    ) -> anyhow::Result<Option<LeasedFetch>> {
        // AssertSqlSafe: assembled from string constants in this file,
        // no external input.
        let sql = sqlx::AssertSqlSafe(claim_due_sql(
            "fetch",
            "",
            "r.slug, r.fetch_depth, f.base_url, f.token_env",
        ));
        let row = sqlx::query_as(sql)
            .bind(lease.as_secs_f64())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Record a successful fetch: the job is done and the repo's sync
    /// position advances to the fetched commit, atomically. Returns
    /// whether the result was applied — `false` means this worker's lease
    /// had lapsed and another claim owns the job now, so the result was
    /// discarded.
    pub async fn complete_fetch(&self, job: &LeasedFetch, commit: &str) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        // Lock order: repos before jobs, like every transaction that
        // touches both (add_repo, complete_index). Mixed orders here
        // deadlock — e.g. holding a jobs entry while waiting on a repo
        // row another completion took first.
        sqlx::query("SELECT 1 FROM repos WHERE id = $1 FOR UPDATE")
            .bind(job.repo_id)
            .execute(&mut *tx)
            .await?;
        if !settle_leased_job(&mut tx, job.job_id, &job.lease_token, true).await? {
            return Ok(false);
        }
        // Every synced commit wants indexing. The index job carries no
        // payload: a worker reads the repo's sync position at claim time.
        // If a job is already in flight this no-ops — a queued job will
        // pick the new commit up at claim, and a leased one is re-queued
        // by complete_index when it notices the repo moved on.
        enqueue_job_unless_in_flight(&mut tx, "index", job.repo_id).await?;
        sqlx::query("UPDATE repos SET last_synced_commit = $1 WHERE id = $2")
            .bind(commit)
            .bind(job.repo_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Record a failed fetch: back the job off exponentially (30s
    /// doubling per attempt, capped at an hour) and keep the error for
    /// `yg admin status`. Jobs retry indefinitely — sync converges on its
    /// own once the cause (rate limit, token rotation) clears. Returns
    /// whether the failure was recorded; `false` means the lease had
    /// lapsed and the job belongs to a newer claim.
    pub async fn fail_fetch(&self, job: &LeasedFetch, error: &str) -> anyhow::Result<bool> {
        self.fail_leased_job(job.job_id, &job.lease_token, error)
            .await
    }

    /// Claim the next due index job under a lease, same protocol as
    /// [`Self::claim_due_fetch`]. Only repos with a synced commit are
    /// claimable — there is nothing to index before the first fetch lands.
    pub async fn claim_due_index(
        &self,
        lease: std::time::Duration,
    ) -> anyhow::Result<Option<LeasedIndex>> {
        // AssertSqlSafe: assembled from string constants in this file,
        // no external input.
        let sql = sqlx::AssertSqlSafe(claim_due_sql(
            "index",
            "AND r.last_synced_commit IS NOT NULL",
            "r.slug, r.last_synced_commit AS commit,
             f.base_url, f.token_env, r.fetch_depth",
        ));
        let row = sqlx::query_as(sql)
            .bind(lease.as_secs_f64())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Record a finished index job: insert the published Shard and swap
    /// the repo's current-Shard pointer to it, atomically. Lease-fenced
    /// like [`Self::complete_fetch`]; returns whether the result was
    /// applied.
    ///
    /// A fetch can land while the job is leased — `complete_fetch`'s
    /// enqueue no-ops against a job that is merely in flight, so it's
    /// this completion that must notice the repo moved past the commit
    /// it indexed and re-queue the same job rather than retiring it.
    pub async fn complete_index(
        &self,
        job: &LeasedIndex,
        shard: ShardRecord<'_>,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        // Lock the repo row before reading its sync position so a
        // concurrent complete_fetch can't advance it between this check
        // and commit (its own repos UPDATE serializes behind this lock).
        let (current_commit,): (Option<String>,) =
            sqlx::query_as("SELECT last_synced_commit FROM repos WHERE id = $1 FOR UPDATE")
                .bind(job.repo_id)
                .fetch_one(&mut *tx)
                .await?;
        let up_to_date = current_commit.as_deref() == Some(job.commit.as_str());
        if !settle_leased_job(&mut tx, job.job_id, &job.lease_token, up_to_date).await? {
            return Ok(false);
        }
        // Revisions are deterministic per (commit, pass, schema), so
        // re-indexing an unchanged repo arrives here with a revision
        // that's already recorded: reuse the row instead of failing.
        // DO UPDATE (with values identical by construction) rather than
        // DO NOTHING so RETURNING yields the id on conflict too.
        let (shard_id,): (i64,) = sqlx::query_as(
            "INSERT INTO shards (repo_id, revision, manifest_key, commit_sha,
                                 provenance_level, node_count, edge_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (repo_id, revision) DO UPDATE
             SET manifest_key = excluded.manifest_key
             RETURNING id",
        )
        .bind(job.repo_id)
        .bind(shard.revision)
        .bind(shard.manifest_key)
        .bind(shard.commit_sha)
        .bind(shard.provenance_level)
        .bind(shard.node_count)
        .bind(shard.edge_count)
        .fetch_one(&mut *tx)
        .await?;
        // Swap the current-Shard pointer. A superseded result still
        // swaps — serving a stale Shard while the re-queued job runs
        // beats serving none — except when the pointer already holds the
        // synced head's Shard: exactly-current must never move backward
        // (a force-push to an older commit racing its own undo).
        //
        // The up_to_date arm assumes one SCHEMA_VERSION in flight: a
        // mixed-image deploy re-indexing an unchanged commit could swap
        // an exactly-current pointer between that commit's per-schema
        // shards in either direction. Accepted for now under the same
        // pre-production stance as migration 0005; ordering would need
        // the shards row to carry its schema version.
        sqlx::query(
            "UPDATE repos SET current_shard_id = $1
             WHERE id = $2
               AND ($3 OR current_shard_id IS NULL OR NOT EXISTS (
                       SELECT 1 FROM shards s
                       WHERE s.id = repos.current_shard_id
                         AND s.commit_sha = repos.last_synced_commit))",
        )
        .bind(shard_id)
        .bind(job.repo_id)
        .bind(up_to_date)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Record a failed index run: back the job off and keep the error
    /// for `yg admin status`, exactly like [`Self::fail_fetch`]. Returns
    /// whether the failure was recorded; `false` means the lease had
    /// lapsed and the job belongs to a newer claim.
    pub async fn fail_index(&self, job: &LeasedIndex, error: &str) -> anyhow::Result<bool> {
        self.fail_leased_job(job.job_id, &job.lease_token, error)
            .await
    }

    /// Re-queue a leased job with exponential backoff (30s doubling per
    /// attempt, capped at an hour), keeping the error for `yg admin
    /// status`. Lease-fenced like completion: `false` means the lease had
    /// lapsed and the failure was discarded.
    async fn fail_leased_job(
        &self,
        job_id: i64,
        lease_token: &str,
        error: &str,
    ) -> anyhow::Result<bool> {
        let applied = sqlx::query(
            "UPDATE jobs
             SET state = 'queued', lease_until = NULL,
                 attempts = attempts + 1, last_error = $2,
                 run_after = now()
                     + make_interval(secs => least(30 * 2 ^ least(attempts, 7), 3600))
             WHERE id = $1 AND state = 'leased' AND lease_until::text = $3",
        )
        .bind(job_id)
        .bind(error)
        .bind(lease_token)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        Ok(applied)
    }

    /// How many repos are indexed into the Knowledge Graph: those whose
    /// current-Shard pointer is set.
    pub async fn indexed_repo_count(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM repos WHERE current_shard_id IS NOT NULL")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    /// Liveness probe used by the server's health endpoint.
    pub async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

/// The one lease-claim query, shared by every job kind so the claim
/// protocol can't drift between kinds. `$1` is the lease in seconds.
/// `state <> 'done'` looks implied by the queued-or-expired OR (done
/// rows have lease_until NULL), but the planner needs the partial index
/// predicate verbatim to use jobs_claim_scan. The lease_until::text
/// rendering is the fencing token settle/fail compare against. Kind,
/// an extra due-predicate (over jobs `j` and repos `r`), and the extra
/// RETURNING columns are the only legitimate variation.
fn claim_due_sql(kind: &str, extra_due_predicate: &str, returning: &str) -> String {
    format!(
        "WITH due AS (
             SELECT j.id FROM jobs j JOIN repos r ON r.id = j.repo_id
             WHERE j.kind = '{kind}' AND j.state <> 'done' AND j.run_after <= now()
               AND (j.state = 'queued' OR j.lease_until < now())
               {extra_due_predicate}
             ORDER BY j.priority DESC, j.run_after
             LIMIT 1
             FOR UPDATE OF j SKIP LOCKED
         )
         UPDATE jobs j
         SET state = 'leased', lease_until = now() + make_interval(secs => $1)
         FROM due, repos r JOIN forges f ON f.id = r.forge_id
         WHERE j.id = due.id AND r.id = j.repo_id
         RETURNING j.id AS job_id, j.repo_id, j.attempts, {returning},
                   j.lease_until::text AS lease_token"
    )
}

/// Queue a job for the repo unless one of its kind is already in flight
/// — jobs_one_in_flight_per_repo_kind dedups in-flight work. Returns
/// whether a new job was queued.
async fn enqueue_job_unless_in_flight(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: &str,
    repo_id: i64,
) -> anyhow::Result<bool> {
    let queued = sqlx::query(
        "INSERT INTO jobs (kind, repo_id) VALUES ($1, $2)
         ON CONFLICT (repo_id, kind) WHERE state <> 'done' DO NOTHING",
    )
    .bind(kind)
    .bind(repo_id)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    Ok(queued)
}

/// Settle a leased job, but only while the caller still holds its lease.
/// `done = true` retires it; otherwise it goes back to the queue as
/// fresh, immediately-due work with the failure bookkeeping cleared —
/// re-queuing is not a failure, and stale attempts would inflate the
/// next backoff and read as "retrying" in admin status.
///
/// The token is the lease's text rendering; comparing in the text
/// domain avoids any text→timestamptz parse-back, so a session formatting
/// quirk can only fail closed (result discarded, job re-runs at lease
/// expiry), never match a stale claim.
async fn settle_leased_job(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    job_id: i64,
    lease_token: &str,
    done: bool,
) -> anyhow::Result<bool> {
    let claimed = sqlx::query(
        "UPDATE jobs
         SET state = CASE WHEN $3 THEN 'done' ELSE 'queued' END,
             attempts = CASE WHEN $3 THEN attempts ELSE 0 END,
             last_error = CASE WHEN $3 THEN last_error ELSE NULL END,
             lease_until = NULL, run_after = now()
         WHERE id = $1 AND state = 'leased' AND lease_until::text = $2",
    )
    .bind(job_id)
    .bind(lease_token)
    .bind(done)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    Ok(claimed)
}
