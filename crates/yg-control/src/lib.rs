//! Postgres models, job queue.

use anyhow::Context;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Where the control plane lives when `YG_DATABASE_URL` says nothing:
/// the in-repo dev compose stack.
pub const DEFAULT_DATABASE_URL: &str = "postgres://yggdrasil:yggdrasil@localhost:5432/yggdrasil";

/// Handle to the control-plane database. The single entry point for
/// everything the Index Server keeps in Postgres.
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
        let fetch_queued = sqlx::query(
            "INSERT INTO jobs (kind, repo_id) VALUES ('fetch', $1)
             ON CONFLICT (repo_id, kind) WHERE state <> 'done' DO NOTHING",
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(AddRepoOutcome {
            repo_id,
            created,
            fetch_queued,
        })
    }

    /// Every registered repo with its sync position: last synced commit
    /// plus the in-flight fetch job, if any.
    pub async fn admin_status(&self) -> anyhow::Result<Vec<RepoSyncStatus>> {
        let rows = sqlx::query_as(
            "SELECT r.slug, f.base_url AS forge, r.last_synced_commit,
                    j.state AS job_state, coalesce(j.attempts, 0) AS attempts,
                    j.last_error
             FROM repos r
             JOIN forges f ON f.id = r.forge_id
             LEFT JOIN LATERAL (
                 SELECT state, attempts, last_error FROM jobs
                 WHERE repo_id = r.id AND kind = 'fetch' AND state <> 'done'
                 ORDER BY id DESC LIMIT 1
             ) j ON true
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
        let row = sqlx::query_as(
            // `state <> 'done'` looks implied by the OR below (done rows
            // have lease_until NULL), but the planner needs the partial
            // index predicate verbatim to use jobs_claim_scan.
            "WITH due AS (
                 SELECT id FROM jobs
                 WHERE kind = 'fetch' AND state <> 'done' AND run_after <= now()
                   AND (state = 'queued' OR lease_until < now())
                 ORDER BY priority DESC, run_after
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE jobs j
             SET state = 'leased', lease_until = now() + make_interval(secs => $1)
             FROM due, repos r JOIN forges f ON f.id = r.forge_id
             WHERE j.id = due.id AND r.id = j.repo_id
             RETURNING j.id AS job_id, j.repo_id, j.attempts,
                       r.slug, r.fetch_depth, f.base_url, f.token_env,
                       j.lease_until::text AS lease_token",
        )
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
        // The token is the lease's text rendering; comparing in the text
        // domain avoids any text→timestamptz parse-back, so a session
        // formatting quirk can only fail closed (result discarded, job
        // re-runs at lease expiry), never match a stale claim.
        let claimed = sqlx::query(
            "UPDATE jobs SET state = 'done', lease_until = NULL
             WHERE id = $1 AND state = 'leased' AND lease_until::text = $2",
        )
        .bind(job.job_id)
        .bind(&job.lease_token)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            return Ok(false);
        }
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
        let applied = sqlx::query(
            "UPDATE jobs
             SET state = 'queued', lease_until = NULL,
                 attempts = attempts + 1, last_error = $2,
                 run_after = now()
                     + make_interval(secs => least(30 * 2 ^ least(attempts, 7), 3600))
             WHERE id = $1 AND state = 'leased' AND lease_until::text = $3",
        )
        .bind(job.job_id)
        .bind(error)
        .bind(&job.lease_token)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        Ok(applied)
    }

    /// How many repos are indexed into the Knowledge Graph.
    pub async fn indexed_repo_count(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM repos WHERE indexed")
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
