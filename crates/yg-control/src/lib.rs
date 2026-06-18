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
    /// Per-repo poll interval in seconds; `None` uses the server default.
    pub poll_interval_seconds: Option<i32>,
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

/// The repo qualifier (RFC 0001 §5): the Forge root sans scheme joined
/// with the slug — `github.com/acme/widgets` — the repo part of every
/// external node id. This is the one definition: computed here when a
/// repo is registered, stored on its row, and matched verbatim by
/// [`ControlPlane::verb_target`]. (yg-verbs owns the matching grammar
/// that extracts it back out of ids.)
pub fn repo_qualifier(base_url: &str, slug: &str) -> String {
    let root = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    // The degenerate forge root (`file:///`) leaves a bare slash; the
    // qualifier joins with exactly one.
    let root = root.trim_end_matches('/');
    format!("{root}/{slug}")
}

/// A registration that collides with an existing repo's qualifier —
/// the same host/slug already registered through a different Forge
/// root (say, http vs https). The caller chose the conflicting URL, so
/// this is theirs to hear about, not a server fault. Detect with
/// `err.downcast_ref::<QualifierConflict>()`.
#[derive(Debug)]
pub struct QualifierConflict {
    pub qualifier: String,
}

impl std::fmt::Display for QualifierConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is already registered through a different forge URL",
            self.qualifier
        )
    }
}

impl std::error::Error for QualifierConflict {}

/// What a Verb request resolves its repo qualifier to: the repo plus its
/// current Shard pointer.
#[derive(sqlx::FromRow)]
pub struct VerbTarget {
    pub repo_id: i64,
    /// Current Shard revision; `None` until the repo is first indexed.
    pub revision: Option<String>,
}

/// One indexed repo in the `search` Verb's fan-out set: its id, the
/// qualifier that prefixes its external node ids, and the current Shard
/// revision to read.
#[derive(Debug, sqlx::FromRow)]
pub struct IndexedRepo {
    pub repo_id: i64,
    pub qualifier: String,
    pub revision: String,
}

/// A repo the poll loop has claimed for a default-branch head check
/// (RFC 0001 §3): everything needed to make the cheap conditional request
/// and, on a moved head, enqueue a fetch. Claiming advances the repo's
/// `next_poll_at` by one jittered interval, so two Sync workers never
/// poll the same repo in one cycle.
#[derive(Debug, sqlx::FromRow)]
pub struct DuePoll {
    pub repo_id: i64,
    pub slug: String,
    /// The forge row, keying the per-forge rate budget.
    pub forge_id: i64,
    /// Forge root; the clone URL is `{base_url}/{slug}`.
    pub base_url: String,
    /// Env var holding the Forge token, if the forge has one.
    pub token_env: Option<String>,
    /// Shallow-clone override; `None` = full history.
    pub fetch_depth: Option<i32>,
    /// Per-repo polling interval override, if configured.
    pub poll_interval_seconds: Option<i32>,
    /// The repo's sync position — the commit a moved head is compared
    /// against. Never NULL: the claim gates on it.
    pub synced_commit: String,
    /// Conditional requests per minute allowed against this forge.
    pub rate_budget: i32,
}

/// A superseded Shard the GC sweep may reclaim: no repo points at it and
/// its grace window has elapsed. Carries what object storage needs to
/// delete the Shard's segments — its repo and revision name the
/// `shards/<repo>/<revision>/` prefix — plus the row id to drop.
#[derive(Debug, sqlx::FromRow)]
pub struct SupersededShard {
    pub shard_id: i64,
    pub repo_id: i64,
    pub revision: String,
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
        // existing row. The qualifier is deterministic from (base_url,
        // slug), so the upsert never changes it; a unique violation on
        // it means the same qualifier arrived via a different forge row.
        let qualifier = repo_qualifier(repo.base_url, repo.slug);
        let inserted: Result<(i64, bool), sqlx::Error> = sqlx::query_as(
            "INSERT INTO repos (forge_id, slug, fetch_depth, qualifier, poll_interval_seconds)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (forge_id, slug) DO UPDATE
             SET fetch_depth = excluded.fetch_depth,
                 poll_interval_seconds = excluded.poll_interval_seconds
             RETURNING id, (xmax = 0)",
        )
        .bind(forge_id)
        .bind(repo.slug)
        .bind(repo.fetch_depth)
        .bind(&qualifier)
        .bind(repo.poll_interval_seconds)
        .fetch_one(&mut *tx)
        .await;
        let (repo_id, created) = match inserted {
            Ok(row) => row,
            Err(sqlx::Error::Database(e)) if e.constraint() == Some("repos_qualifier") => {
                return Err(anyhow::Error::new(QualifierConflict { qualifier }));
            }
            Err(e) => return Err(e.into()),
        };
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

    /// Claim the most-overdue repo for a poll, advancing its `next_poll_at`
    /// by one jittered interval — in `[interval, interval·(1 + jitter)]`,
    /// the spread keeping a forge's repos off lockstep. `FOR UPDATE SKIP
    /// LOCKED` lets parallel Sync workers each claim a different repo;
    /// advancing the schedule at claim time is the claim, so a crashed
    /// poll just costs one skipped cycle. Only synced repos are eligible
    /// — a repo's first fetch is queued at registration, so there is
    /// nothing to compare a head against before it lands. The per-repo
    /// `poll_interval_seconds` overrides the default. Returns `None` when
    /// nothing is due.
    pub async fn claim_due_poll(
        &self,
        default_interval: std::time::Duration,
        jitter_fraction: f64,
    ) -> anyhow::Result<Option<DuePoll>> {
        let row = sqlx::query_as(
            "WITH due AS (
                 SELECT r.id FROM repos r
                 WHERE r.last_synced_commit IS NOT NULL AND r.next_poll_at <= now()
                 ORDER BY r.next_poll_at
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             UPDATE repos r
             SET next_poll_at = now()
                 + make_interval(secs => coalesce(r.poll_interval_seconds, $1)
                                         * (1 + $2 * random()))
             FROM due, forges f
             WHERE r.id = due.id AND f.id = r.forge_id
             RETURNING r.id AS repo_id, r.slug, f.id AS forge_id,
                       f.base_url, f.token_env, r.fetch_depth,
                       r.poll_interval_seconds,
                       r.last_synced_commit AS synced_commit, f.rate_budget",
        )
        .bind(default_interval.as_secs_f64())
        .bind(jitter_fraction)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Reschedule a repo's next poll `delay` from now, overriding the
    /// interval the claim set. The poll loop calls this when it must skip
    /// a due repo it has already claimed — the forge is over its rate
    /// budget or cooling down — so the repo retries when a request is
    /// available again rather than waiting a full interval.
    pub async fn defer_poll(&self, repo_id: i64, delay: std::time::Duration) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE repos SET next_poll_at = now() + make_interval(secs => $2) WHERE id = $1",
        )
        .bind(repo_id)
        .bind(delay.as_secs_f64())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Enqueue a fetch for a repo whose default branch the poll loop saw
    /// move, unless one of its fetches is already in flight (a queued or
    /// leased fetch already covers the change). The fetch then re-syncs
    /// the repo and the existing pipeline re-indexes it. Returns whether
    /// a new job was queued.
    pub async fn request_fetch(&self, repo_id: i64) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let queued = enqueue_job_unless_in_flight(&mut tx, "fetch", repo_id).await?;
        tx.commit().await?;
        Ok(queued)
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
        let (current_commit, old_shard_id): (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT last_synced_commit, current_shard_id FROM repos WHERE id = $1 FOR UPDATE",
        )
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
        // `published_at` is refreshed too: republishing a revision (a
        // force-push back to its commit, churn) re-publishes the Shard, so
        // its GC grace window must restart from now — otherwise a revision
        // resurrected long after its first publish, but left non-current,
        // would carry a stale publish time and be eligible for GC with no
        // grace (its anchor is `published_at` until it is superseded).
        let (shard_id,): (i64,) = sqlx::query_as(
            "INSERT INTO shards (repo_id, revision, manifest_key, commit_sha,
                                 provenance_level, node_count, edge_count)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (repo_id, revision) DO UPDATE
             SET manifest_key = excluded.manifest_key, published_at = now()
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
        let swapped: Option<(i64,)> = sqlx::query_as(
            "UPDATE repos SET current_shard_id = $1
             WHERE id = $2
               AND ($3 OR current_shard_id IS NULL OR NOT EXISTS (
                       SELECT 1 FROM shards s
                       WHERE s.id = repos.current_shard_id
                         AND s.commit_sha = repos.last_synced_commit))
             RETURNING current_shard_id",
        )
        .bind(shard_id)
        .bind(job.repo_id)
        .bind(up_to_date)
        .fetch_optional(&mut *tx)
        .await?;
        // The pointer moved: the Shard it left is now superseded — stamp
        // when, so GC's grace window runs from supersession, not
        // publication. The Shard it landed on is current, so clear any
        // stamp it carried (a revision re-published after being
        // superseded becomes current again). No move (an exactly-current
        // pointer held its ground) supersedes nothing.
        if swapped.is_some() {
            sqlx::query("UPDATE shards SET superseded_at = NULL WHERE id = $1")
                .bind(shard_id)
                .execute(&mut *tx)
                .await?;
            if let Some(old) = old_shard_id.filter(|old| *old != shard_id) {
                sqlx::query("UPDATE shards SET superseded_at = now() WHERE id = $1")
                    .bind(old)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Queue a re-index for every repo whose current Shard's revision
    /// doesn't end with `current_suffix` — the pass+schema suffix every
    /// revision id this binary publishes carries. After a deploy that
    /// bumps the Shard schema, the read path refuses the old artifacts
    /// (they predate the schema this binary reads), and this — run at
    /// worker boot — is what re-converges the fleet: revisions are
    /// deterministic, so each re-index publishes the new revision
    /// idempotently and swaps the pointer. Returns how many repos were
    /// queued; repos with a job already in flight count as covered.
    pub async fn requeue_outdated_shards(&self, current_suffix: &str) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin().await?;
        let outdated: Vec<(i64,)> = sqlx::query_as(
            "SELECT r.id FROM repos r
             JOIN shards s ON s.id = r.current_shard_id
             WHERE r.last_synced_commit IS NOT NULL
               AND right(s.revision, length($1)) <> $1
             ORDER BY r.id",
        )
        .bind(current_suffix)
        .fetch_all(&mut *tx)
        .await?;
        let mut queued = 0;
        for (repo_id,) in outdated {
            if enqueue_job_unless_in_flight(&mut tx, "index", repo_id).await? {
                queued += 1;
            }
        }
        tx.commit().await?;
        Ok(queued)
    }

    /// Every Shard the GC sweep may reclaim: one no repo points at,
    /// superseded (or, for a result that never became current, published)
    /// longer ago than `grace`. The grace window runs from supersession,
    /// not publication, so a query that resolved the old pointer just
    /// before a swap keeps its Shard while it reads. Ordered by id for a
    /// stable, resumable sweep.
    pub async fn superseded_shards_past_grace(
        &self,
        grace: std::time::Duration,
    ) -> anyhow::Result<Vec<SupersededShard>> {
        let rows = sqlx::query_as(
            "SELECT s.id AS shard_id, s.repo_id, s.revision FROM shards s
             WHERE NOT EXISTS (SELECT 1 FROM repos r WHERE r.current_shard_id = s.id)
               AND coalesce(s.superseded_at, s.published_at) < now() - make_interval(secs => $1)
             ORDER BY s.id",
        )
        .bind(grace.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Claim a superseded Shard's row for reclamation — delete it, but
    /// only while no repo points at it. Revisions are deterministic per
    /// `(commit, pass, schema)`, so a force-push back to a Shard's commit
    /// can republish that exact revision and re-point a repo at this very
    /// row between the GC sweep's eligibility scan and this delete; the
    /// `NOT EXISTS` guard makes that case a no-op instead of a foreign-key
    /// error (which would wedge the sweep, retrying forever). Returns
    /// whether the row was deleted — `false` means it became current
    /// again and its object-storage segments must be left alone.
    pub async fn delete_superseded_shard(&self, shard_id: i64) -> anyhow::Result<bool> {
        let deleted = sqlx::query(
            "DELETE FROM shards s
             WHERE s.id = $1
               AND NOT EXISTS (SELECT 1 FROM repos r WHERE r.current_shard_id = s.id)",
        )
        .bind(shard_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        Ok(deleted)
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

    /// Resolve a repo qualifier (see [`repo_qualifier`]) to the repo and
    /// its current Shard pointer. Resolved per query, so a pointer swap
    /// is picked up by the very next request, no restart.
    pub async fn verb_target(&self, qualifier: &str) -> anyhow::Result<Option<VerbTarget>> {
        let target = sqlx::query_as(
            "SELECT r.id AS repo_id, s.revision FROM repos r
             LEFT JOIN shards s ON s.id = r.current_shard_id
             WHERE r.qualifier = $1",
        )
        .bind(qualifier)
        .fetch_optional(&self.pool)
        .await
        .context("resolving the repo qualifier")?;
        Ok(target)
    }

    /// Every indexed repo whose current Shard is at the caller's revision
    /// suffix — the fan-out set the lexical `search` Verb queries when no
    /// `repos` filter narrows it (RFC 0001 §7). The suffix encodes the pass
    /// and schema version (`-syntactic-v4`), so a repo still pointing at an
    /// older-schema Shard mid-migration is simply absent (it has no
    /// current-schema segment to search yet), rather than failing the whole
    /// org-wide search. Resolved per query, so a newly indexed repo joins
    /// the next search and a pointer swap is picked up without a restart.
    pub async fn indexed_repos(&self, revision_suffix: &str) -> anyhow::Result<Vec<IndexedRepo>> {
        let repos = sqlx::query_as(
            "SELECT r.id AS repo_id, r.qualifier, s.revision FROM repos r
             JOIN shards s ON s.id = r.current_shard_id
             WHERE right(s.revision, length($1)) = $1
             ORDER BY r.qualifier",
        )
        .bind(revision_suffix)
        .fetch_all(&self.pool)
        .await
        .context("listing indexed repos for search")?;
        Ok(repos)
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

#[cfg(test)]
mod tests {
    use super::repo_qualifier;

    #[test]
    fn qualifiers_join_the_schemeless_forge_root_with_the_slug() {
        assert_eq!(
            repo_qualifier("https://github.com", "acme/widgets"),
            "github.com/acme/widgets"
        );
        assert_eq!(
            repo_qualifier("https://git.corp.example:8443", "acme/widgets"),
            "git.corp.example:8443/acme/widgets"
        );
        // The degenerate file root ("file:///", RepoLocator's base for a
        // two-segment path) must not double the slash.
        assert_eq!(repo_qualifier("file:///", "srv/repo"), "/srv/repo");
        assert_eq!(
            repo_qualifier("file:///tmp/fixtures", "acme/widgets"),
            "/tmp/fixtures/acme/widgets"
        );
    }
}
