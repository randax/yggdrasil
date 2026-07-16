//! Postgres models, job queue.

pub mod metrics;
pub use metrics::{JobOutcome, JobTimer, Metrics};

use anyhow::Context;
use sqlx::PgPool;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgPoolOptions;

/// Where the control plane lives when `YG_DATABASE_URL` says nothing:
/// the in-repo dev compose stack.
pub const DEFAULT_DATABASE_URL: &str = "postgres://yggdrasil:yggdrasil@localhost:5432/yggdrasil";

/// Handle to the control-plane database. The single entry point for
/// everything the Index Server keeps in Postgres. Clones share one pool.
#[derive(Clone)]
pub struct ControlPlane {
    pool: PgPool,
    metrics: Metrics,
}

/// Exclusive ownership of one Shard revision's object/control-plane
/// transition. Session advisory locks make the publication critical section
/// and reclamation mutually exclusive across worker processes. Cancellation
/// closes the checked-out connection, which makes Postgres release the lock.
pub struct ShardOperationGuard {
    connection: Option<PoolConnection<sqlx::Postgres>>,
    key: i64,
}

impl ShardOperationGuard {
    /// Release the advisory lock and return its dedicated connection to the
    /// pool. A failed unlock closes the connection instead, so a session lock
    /// can never leak back into the pool.
    pub async fn release(mut self) {
        if let Some(connection) = self.connection.as_mut() {
            let unlocked = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
                .bind(self.key)
                .fetch_one(&mut **connection)
                .await;
            if matches!(unlocked, Ok(true)) {
                // Disarm Drop so the unlocked session returns to the pool.
                self.connection.take();
            }
        }
    }
}

/// A held Shard-operation fence. This small seam lets orchestration tests
/// exercise the same lifetime rule as the Postgres advisory-lock guard.
pub trait ShardOperationFence: Send {
    fn release(self) -> impl std::future::Future<Output = ()> + Send;
}

impl ShardOperationFence for ShardOperationGuard {
    async fn release(self) {
        ShardOperationGuard::release(self).await;
    }
}

/// Run a Shard object/control transition while retaining its exclusive
/// fence, releasing it only after the transition future has completed.
pub async fn finish_shard_operation<Fence, Operation>(
    fence: Fence,
    operation: Operation,
) -> Operation::Output
where
    Fence: ShardOperationFence,
    Operation: std::future::Future,
{
    let result = operation.await;
    fence.release().await;
    result
}

impl Drop for ShardOperationGuard {
    fn drop(&mut self) {
        if let Some(connection) = &mut self.connection {
            connection.close_on_drop();
        }
    }
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
    /// REST API root for the forge's discovery API, if it has one —
    /// e.g. `https://api.github.com`.
    pub api_root: Option<&'a str>,
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

/// A forge org/group connection that discovery should keep reconciling.
pub struct ConnectForgeOrg<'a> {
    /// Forge kind: `github` for issue #10.
    pub forge_kind: &'a str,
    /// Forge root, unique per forge, e.g. `https://github.com`.
    pub base_url: &'a str,
    /// Org or group slug on the forge.
    pub org_slug: &'a str,
    /// Env var the Forge token is read from.
    pub token_env: Option<&'a str>,
    /// REST API root the forge's discovery listing calls — e.g.
    /// `https://api.github.com`, or a test fixture server.
    pub api_root: Option<&'a str>,
}

pub struct ConnectForgeOrgOutcome {
    pub forge_id: i64,
    pub org_id: i64,
    pub forge_kind: StoredForgeKind,
    pub created: bool,
}

/// The forge kind read from a persisted forge record.
pub struct StoredForgeKind(String);

impl StoredForgeKind {
    fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One connected org due for repository discovery.
#[derive(Debug, sqlx::FromRow)]
pub struct DueDiscovery {
    pub org_id: i64,
    pub forge_id: i64,
    /// Maximum Forge requests per minute for this worker process.
    pub rate_budget: i32,
    pub forge_kind: String,
    pub base_url: String,
    /// REST API root from the Forge record; `None` when the record
    /// predates the field (re-adding the forge backfills it).
    pub api_root: Option<String>,
    pub org_slug: String,
    pub token_env: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum RepoVisibility {
    Public,
    Internal,
    Private,
}

impl RepoVisibility {
    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Private => "private",
        }
    }
}

/// Lifecycle of a Shard registry row. Published rows may be pointed at;
/// reclaiming rows are owned by GC and fence publication until object
/// cleanup finishes and the row is reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ShardState {
    Published,
    Reclaiming,
}

impl ShardState {
    #[cfg(test)]
    const ALL: [Self; 2] = [Self::Published, Self::Reclaiming];

    fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Reclaiming => "reclaiming",
        }
    }
}

pub struct DiscoveredRepo<'a> {
    pub slug: &'a str,
    pub visibility: RepoVisibility,
    pub fetch_depth: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Include,
    Exclude,
}

impl RuleAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Include => "include",
            Self::Exclude => "exclude",
        }
    }
}

/// The job queue's kind vocabulary. Mirrored by the `jobs_kind_check`
/// constraint (migration 0011), so a typo can't mint a row no claim
/// query will ever match; a new kind lands as a variant here plus a
/// migration extending the constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Fetch,
    Index,
}

impl JobKind {
    /// Every kind, in one place, so tests can assert the database
    /// constraint accepts exactly this vocabulary.
    pub const ALL: [JobKind; 2] = [JobKind::Fetch, JobKind::Index];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Index => "index",
        }
    }

    fn from_database(value: &str) -> Option<Self> {
        match value {
            "fetch" => Some(Self::Fetch),
            "index" => Some(Self::Index),
            _ => None,
        }
    }
}

pub struct AddRule<'a> {
    pub forge_id: i64,
    pub pattern: &'a str,
    pub action: RuleAction,
    pub applies_to_private: bool,
}

pub struct AddRuleOutcome {
    pub created: bool,
    pub repos_reconsidered: u64,
    pub fetches_queued: u64,
}

pub struct IssuedMemberToken {
    pub id: String,
    pub member: String,
    /// The bearer token material. Returned only from issue time; only its
    /// hash is persisted.
    pub token: String,
}

/// Member token ids are URL path components and operator-facing handles,
/// so keep their grammar narrow and stable.
pub fn member_token_id_is_valid(id: &str) -> bool {
    let Some(hex) = id.strip_prefix("mtok_") else {
        return false;
    };
    hex.len() == 24 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, sqlx::FromRow)]
pub struct DiscoveryRule {
    pub forge: String,
    pub pattern: String,
    pub action: String,
    pub applies_to_private: bool,
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
    /// Forge kind, e.g. `github` — selects the worker's forge adapter.
    pub forge_kind: String,
    /// Forge root; the clone URL is `{base_url}/{slug}`.
    pub base_url: String,
    /// Env var holding the Forge token, if the forge has one.
    pub token_env: Option<String>,
    /// Opaque fencing token for this claim. `complete_fetch`/`fail_fetch`
    /// only apply while it still matches — a worker that outlived its
    /// lease (the job was re-claimed) has its result discarded.
    #[sqlx(try_from = "String")]
    lease_token: LeaseToken,
    claim_latency_seconds: f64,
}

/// The claim's fencing token. The token is the lease deadline's text
/// rendering, so a successful renewal mints a new one; interior
/// mutability lets the heartbeat swap it in while the work future holds
/// a shared borrow of the same leased job. The mutex is uncontended —
/// one worker task renews and settles sequentially — and no code panics
/// while holding it, so poisoning is unreachable.
struct LeaseToken(std::sync::Mutex<String>);

impl LeaseToken {
    fn current(&self) -> String {
        self.0.lock().expect("lease token lock poisoned").clone()
    }

    fn replace(&self, token: String) {
        *self.0.lock().expect("lease token lock poisoned") = token;
    }
}

impl From<String> for LeaseToken {
    fn from(token: String) -> Self {
        Self(std::sync::Mutex::new(token))
    }
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
    /// Forge kind, e.g. `github` — selects the worker's forge adapter.
    pub forge_kind: String,
    /// Forge root; the clone URL is `{base_url}/{slug}`.
    pub base_url: String,
    /// Env var holding the Forge token, if the forge has one.
    pub token_env: Option<String>,
    /// Shallow-clone override; `None` = full history.
    pub fetch_depth: Option<i32>,
    /// Opaque fencing token; see [`LeasedFetch::lease_token`].
    #[sqlx(try_from = "String")]
    lease_token: LeaseToken,
    claim_latency_seconds: f64,
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

/// [`repo_qualifier`], rendered as SQL over a forge row `f` and a repos
/// row `r` — what migration 0011 re-derives stored qualifiers with. The
/// tests pin this expression to the Rust function (same output over a
/// corpus of forge roots) and to the migration file (verbatim
/// containment), so the grammar cannot drift in either direction.
pub const REPO_QUALIFIER_SQL: &str = "rtrim(CASE WHEN strpos(f.base_url, '://') > 0
                           THEN substr(f.base_url, strpos(f.base_url, '://') + 3)
                           ELSE f.base_url END, '/') || '/' || r.slug";

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
    /// Forge kind, e.g. `github` — selects the worker's forge adapter.
    pub forge_kind: String,
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
    /// Seconds this repo was overdue when claimed for polling.
    pub poll_lag_seconds: f64,
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
    pub state: ShardState,
}

/// One repo's row in `yg admin status`.
#[derive(sqlx::FromRow)]
pub struct RepoSyncStatus {
    pub slug: String,
    /// The forge's base URL.
    pub forge: String,
    pub visibility: RepoVisibility,
    pub discovery_state: String,
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

#[derive(sqlx::FromRow)]
struct RuleRow {
    pattern: String,
    action: String,
    applies_to_private: bool,
}

impl ControlPlane {
    /// Serialize the object publication critical section and reclamation for
    /// one deterministic revision across every worker process. Publishers do
    /// fetch, checkout, and parsing before taking this lock, then hold it while
    /// re-checking Shard state and existing publication, writing objects, and
    /// completing the control-plane transition. Reclaimers hold it through
    /// their object deletion and control-plane transition.
    ///
    /// This session advisory lock is coordination, not a hard object-store
    /// fence: if its Postgres session drops, the server releases the lock even
    /// while the Rust future may still be running. That residual race is
    /// accepted here because publication's locked section is short, puts are
    /// create-only, and revision identities are deterministic. A true hard
    /// fence would require an object-store-visible fencing-token design.
    pub async fn lock_shard_operation(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<ShardOperationGuard> {
        use sha2::Digest;

        let digest = sha2::Sha256::digest(format!("{repo_id}\0{revision}").as_bytes());
        let key = i64::from_be_bytes(digest[..8].try_into().expect("sha256 has eight bytes"));
        let connection = self.pool.acquire().await?;
        // A cancelled waiter must close its session: Postgres may grant a
        // blocking advisory-lock request just as the Rust future drops.
        // Returning that session to the pool could strand an ownerless
        // lock indefinitely.
        let mut operation = ShardOperationGuard {
            connection: Some(connection),
            key,
        };
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(key)
            .execute(
                &mut **operation
                    .connection
                    .as_mut()
                    .expect("Shard operation guard owns its connection"),
            )
            .await?;
        Ok(operation)
    }

    /// State of a deterministic Shard revision. Callers coordinating
    /// publication hold [`ShardOperationGuard`] while consulting it.
    pub async fn shard_state(
        &self,
        repo_id: i64,
        revision: &str,
    ) -> anyhow::Result<Option<ShardState>> {
        let state =
            sqlx::query_scalar("SELECT state FROM shards WHERE repo_id = $1 AND revision = $2")
                .bind(repo_id)
                .bind(revision)
                .fetch_optional(&self.pool)
                .await?;
        Ok(state)
    }

    /// Return a leased index job to the queue without recording a
    /// failure. Used when its deterministic revision is being reclaimed.
    pub async fn defer_index_for_reclamation(&self, job: &LeasedIndex) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let deferred = defer_leased_index_for_reclamation(&mut tx, job).await?;
        tx.commit().await?;
        Ok(deferred)
    }

    /// Connect and bring the schema up to date. Applied migrations are
    /// tracked in `_sqlx_migrations`, so restarting against an
    /// already-migrated database is a no-op.
    pub async fn connect_and_migrate(database_url: &str) -> anyhow::Result<Self> {
        Self::connect_and_migrate_with_metrics(database_url, Metrics::unregistered()).await
    }

    /// Connect using job collectors supplied by the process composition root.
    pub async fn connect_and_migrate_with_metrics(
        database_url: &str,
        metrics: Metrics,
    ) -> anyhow::Result<Self> {
        // Sizing invariant: shard-operation guards each pin one dedicated
        // connection while the same task acquires a second, transient one
        // for coordinated SQL. The worker wiring runs at most one index
        // job and one GC sweep concurrently (two guards, four connections
        // worst case), so five suffices. Raising worker concurrency
        // requires raising this bound with it.
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .context("connecting to control-plane Postgres")?;
        MIGRATOR
            .run(&pool)
            .await
            .context("running control-plane migrations")?;
        Ok(Self { pool, metrics })
    }

    /// Register a repository for Sync: upsert its Forge, the repo row,
    /// and an exact-slug include rule, and queue a fetch job unless one
    /// is already in flight. Idempotent — re-adding an existing repo
    /// changes nothing but its depth override (and re-queues a fetch if
    /// none is pending).
    pub async fn add_repo(&self, repo: AddRepo<'_>) -> anyhow::Result<AddRepoOutcome> {
        let mut tx = self.pool.begin().await?;
        // DO UPDATE (rather than DO NOTHING) so RETURNING yields the id
        // on conflict too. The existing kind always wins: the URL
        // locator can only recognize well-known hosts, so a repo add on
        // a GitHub Enterprise forge arrives as generic `git` — letting
        // it overwrite the row `forge add` registered as `github` would
        // silently disable the org's discovery. token_env and api_root
        // only backfill missing values — an explicit per-forge value is
        // never clobbered by a re-add.
        let (forge_id,): (i64,) = sqlx::query_as(
            "INSERT INTO forges (kind, base_url, token_env, api_root) VALUES ($1, $2, $3, $4)
             ON CONFLICT (base_url) DO UPDATE
             SET kind = forges.kind,
                 token_env = coalesce(forges.token_env, excluded.token_env),
                 api_root = coalesce(forges.api_root, excluded.api_root)
             RETURNING id",
        )
        .bind(repo.forge_kind)
        .bind(repo.base_url)
        .bind(repo.token_env)
        .bind(repo.api_root)
        .fetch_one(&mut *tx)
        .await?;
        put_rule_newest(&mut tx, forge_id, repo.slug, RuleAction::Include, true).await?;
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
        let rules = rules_for_forge(&mut tx, forge_id).await?;
        let (visibility,): (RepoVisibility,) =
            sqlx::query_as("SELECT visibility FROM repos WHERE id = $1 FOR UPDATE")
                .bind(repo_id)
                .fetch_one(&mut *tx)
                .await?;
        let discovery_state = discovery_state_for(repo.slug, visibility, &rules);
        sqlx::query("UPDATE repos SET discovery_state = $2 WHERE id = $1")
            .bind(repo_id)
            .bind(discovery_state)
            .execute(&mut *tx)
            .await?;
        let fetch_queued = if discovery_state == "included" {
            enqueue_job_unless_in_flight(&mut tx, JobKind::Fetch, repo_id).await?
        } else {
            false
        };
        tx.commit().await?;
        Ok(AddRepoOutcome {
            repo_id,
            created,
            fetch_queued,
        })
    }

    /// Connect a Forge org/group for recurring discovery. Idempotent:
    /// reconnecting the same org refreshes the token env without
    /// duplicating the connection.
    pub async fn connect_forge_org(
        &self,
        org: ConnectForgeOrg<'_>,
    ) -> anyhow::Result<ConnectForgeOrgOutcome> {
        let mut tx = self.pool.begin().await?;
        let (forge_id, forge_kind): (i64, String) = sqlx::query_as(
            "INSERT INTO forges (kind, base_url, token_env, api_root) VALUES ($1, $2, $3, $4)
             ON CONFLICT (base_url) DO UPDATE
             SET kind = excluded.kind,
                 token_env = coalesce(excluded.token_env, forges.token_env),
                 api_root = coalesce(excluded.api_root, forges.api_root)
             RETURNING id, kind",
        )
        .bind(org.forge_kind)
        .bind(org.base_url)
        .bind(org.token_env)
        .bind(org.api_root)
        .fetch_one(&mut *tx)
        .await?;
        let (org_id, created): (i64, bool) = sqlx::query_as(
            "INSERT INTO forge_orgs (forge_id, org_slug, token_env) VALUES ($1, $2, $3)
             ON CONFLICT (forge_id, org_slug) DO UPDATE
             SET token_env = coalesce(excluded.token_env, forge_orgs.token_env)
             RETURNING id, (xmax = 0)",
        )
        .bind(forge_id)
        .bind(org.org_slug)
        .bind(org.token_env)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ConnectForgeOrgOutcome {
            forge_id,
            org_id,
            forge_kind: StoredForgeKind::new(forge_kind),
            created,
        })
    }

    /// Claim one org whose discovery schedule is due, advancing it by
    /// `interval` before the caller lists the Forge. A crashed discovery
    /// therefore costs at most one interval; manual/on-demand callers use
    /// [`Self::discover_forge_repos`] directly.
    pub async fn claim_due_discovery(
        &self,
        interval: std::time::Duration,
    ) -> anyhow::Result<Option<DueDiscovery>> {
        let row = sqlx::query_as(
            "WITH due AS (
                 SELECT id FROM forge_orgs
                 WHERE next_discovery_at <= now()
                 ORDER BY next_discovery_at
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             UPDATE forge_orgs o
             SET next_discovery_at = now() + make_interval(secs => $1)
             FROM due, forges f
             WHERE o.id = due.id AND f.id = o.forge_id
             RETURNING o.id AS org_id, f.id AS forge_id, f.rate_budget,
                       f.kind AS forge_kind,
                       f.base_url, f.api_root, o.org_slug,
                       coalesce(o.token_env, f.token_env) AS token_env",
        )
        .bind(interval.as_secs_f64())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Reconcile the repositories returned by one Forge org discovery.
    /// Public/internal repos are included by default. Private repos stay
    /// discovered-only unless the winning rule explicitly applies to
    /// private repos and includes them.
    pub async fn discover_forge_repos(
        &self,
        org_id: i64,
        repos: &[DiscoveredRepo<'_>],
    ) -> anyhow::Result<u64> {
        let mut tx = self.pool.begin().await?;
        let (forge_id, base_url): (i64, String) = sqlx::query_as(
            "SELECT f.id, f.base_url
             FROM forge_orgs o JOIN forges f ON f.id = o.forge_id
             WHERE o.id = $1
             FOR UPDATE",
        )
        .bind(org_id)
        .fetch_one(&mut *tx)
        .await?;
        let rules = rules_for_forge(&mut tx, forge_id).await?;
        let mut fetches_queued = 0;
        for repo in repos {
            let state = discovery_state_for(repo.slug, repo.visibility, &rules);
            let qualifier = repo_qualifier(&base_url, repo.slug);
            let existing: Option<(i64, String)> = sqlx::query_as(
                "SELECT id, discovery_state
                 FROM repos
                 WHERE forge_id = $1 AND slug = $2
                 FOR UPDATE",
            )
            .bind(forge_id)
            .bind(repo.slug)
            .fetch_optional(&mut *tx)
            .await?;
            let (repo_id, was_included) = match existing {
                Some((repo_id, previous_state)) => {
                    sqlx::query(
                        "UPDATE repos
                         SET visibility = $2,
                             discovery_state = $3,
                             fetch_depth = coalesce($4, fetch_depth)
                         WHERE id = $1",
                    )
                    .bind(repo_id)
                    .bind(repo.visibility.as_str())
                    .bind(state)
                    .bind(repo.fetch_depth)
                    .execute(&mut *tx)
                    .await?;
                    (repo_id, previous_state == "included")
                }
                None => {
                    let inserted: Option<(i64,)> = sqlx::query_as(
                        "INSERT INTO repos
                            (forge_id, slug, visibility, discovery_state, fetch_depth, qualifier)
                         VALUES ($1, $2, $3, $4, $5, $6)
                         ON CONFLICT (forge_id, slug) DO NOTHING
                         RETURNING id",
                    )
                    .bind(forge_id)
                    .bind(repo.slug)
                    .bind(repo.visibility.as_str())
                    .bind(state)
                    .bind(repo.fetch_depth)
                    .bind(&qualifier)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| match e {
                        sqlx::Error::Database(db) if db.constraint() == Some("repos_qualifier") => {
                            anyhow::Error::new(QualifierConflict { qualifier })
                        }
                        other => other.into(),
                    })?;
                    match inserted {
                        Some((repo_id,)) => (repo_id, false),
                        None => {
                            let (repo_id, previous_state): (i64, String) = sqlx::query_as(
                                "SELECT id, discovery_state
                                 FROM repos
                                 WHERE forge_id = $1 AND slug = $2
                                 FOR UPDATE",
                            )
                            .bind(forge_id)
                            .bind(repo.slug)
                            .fetch_one(&mut *tx)
                            .await?;
                            sqlx::query(
                                "UPDATE repos
                                 SET visibility = $2,
                                     discovery_state = $3,
                                     fetch_depth = coalesce($4, fetch_depth)
                                 WHERE id = $1",
                            )
                            .bind(repo_id)
                            .bind(repo.visibility.as_str())
                            .bind(state)
                            .bind(repo.fetch_depth)
                            .execute(&mut *tx)
                            .await?;
                            (repo_id, previous_state == "included")
                        }
                    }
                }
            };
            if state == "included"
                && !was_included
                && enqueue_job_unless_in_flight(&mut tx, JobKind::Fetch, repo_id).await?
            {
                fetches_queued += 1;
            } else if state != "included" && was_included {
                remove_repo_from_indexing(&mut tx, repo_id).await?;
            }
        }
        sqlx::query("UPDATE forge_orgs SET last_discovered_at = now() WHERE id = $1")
            .bind(org_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(fetches_queued)
    }

    /// Add an include/exclude rule and immediately re-evaluate known
    /// repos for that forge. Precedence is centralized in the
    /// discovery-state evaluator and documented in the ADR/RFC docs.
    pub async fn add_rule(&self, rule: AddRule<'_>) -> anyhow::Result<AddRuleOutcome> {
        let mut tx = self.pool.begin().await?;
        lock_forge_for_update(&mut tx, rule.forge_id).await?;
        let created = put_rule_newest(
            &mut tx,
            rule.forge_id,
            rule.pattern,
            rule.action,
            rule.applies_to_private,
        )
        .await?;

        let rules = rules_for_forge(&mut tx, rule.forge_id).await?;
        let repos: Vec<(i64, String, RepoVisibility, String)> = sqlx::query_as(
            "SELECT id, slug, visibility, discovery_state
             FROM repos
             WHERE forge_id = $1
             ORDER BY id",
        )
        .bind(rule.forge_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut fetches_queued = 0;
        for (repo_id, slug, visibility, previous_state) in &repos {
            let was_included = previous_state == "included";
            let state = discovery_state_for(slug, *visibility, &rules);
            sqlx::query("UPDATE repos SET discovery_state = $2 WHERE id = $1")
                .bind(repo_id)
                .bind(state)
                .execute(&mut *tx)
                .await?;
            if state == "included"
                && !was_included
                && enqueue_job_unless_in_flight(&mut tx, JobKind::Fetch, *repo_id).await?
            {
                fetches_queued += 1;
            } else if state != "included" && was_included {
                remove_repo_from_indexing(&mut tx, *repo_id).await?;
            }
        }
        let repos_reconsidered = repos.len() as u64;
        tx.commit().await?;
        Ok(AddRuleOutcome {
            created,
            repos_reconsidered,
            fetches_queued,
        })
    }

    pub async fn forge_id_by_base_url(&self, base_url: &str) -> anyhow::Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM forges WHERE base_url = $1")
            .bind(base_url)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(id,)| id))
    }

    pub async fn request_discovery(
        &self,
        base_url: &str,
        org_slug: &str,
    ) -> anyhow::Result<Option<StoredForgeKind>> {
        let row: Option<(String,)> = sqlx::query_as(
            "UPDATE forge_orgs o
             SET next_discovery_at = now()
             FROM forges f
             WHERE f.id = o.forge_id AND f.base_url = $1 AND o.org_slug = $2
             RETURNING f.kind",
        )
        .bind(base_url)
        .bind(org_slug)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(kind,)| StoredForgeKind::new(kind)))
    }

    pub async fn rules(&self) -> anyhow::Result<Vec<DiscoveryRule>> {
        let rows = sqlx::query_as(
            "SELECT f.base_url AS forge, r.pattern, r.action, r.applies_to_private
             FROM rules r JOIN forges f ON f.id = r.forge_id
             ORDER BY f.base_url, length(r.pattern) DESC, r.created_at DESC, r.id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Issue a bearer token for a human or agent Member. The plaintext
    /// token is returned to the caller once; only its SHA-256 hash is
    /// recorded.
    pub async fn issue_member_token(&self, member: &str) -> anyhow::Result<IssuedMemberToken> {
        let member = member.trim();
        anyhow::ensure!(!member.is_empty(), "member must not be empty");

        for _ in 0..3 {
            let id = format!("mtok_{}", random_hex(12)?);
            let token = format!("ygm_{}", random_hex(32)?);
            debug_assert!(member_token_id_is_valid(&id));
            let token_hash = hash_member_token(&token);
            let inserted = sqlx::query(
                "INSERT INTO member_tokens (id, member, token_hash)
                 VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
            )
            .bind(&id)
            .bind(member)
            .bind(&token_hash)
            .execute(&self.pool)
            .await?
            .rows_affected()
                == 1;
            if inserted {
                return Ok(IssuedMemberToken {
                    id,
                    member: member.to_string(),
                    token,
                });
            }
        }

        anyhow::bail!("could not allocate a unique member token id")
    }

    /// Revoke one member token by its stable id. Returns false when the
    /// id is unknown or already revoked.
    pub async fn revoke_member_token(&self, id: &str) -> anyhow::Result<bool> {
        let revoked = sqlx::query(
            "UPDATE member_tokens
             SET revoked_at = now()
             WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.trim())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        Ok(revoked)
    }

    /// Validate a presented member bearer token. `last_used_at` is
    /// stamped on first use and then at most once a minute, so hot Verb
    /// traffic does not turn every read request into a write.
    pub async fn authenticate_member_token(&self, token: &str) -> anyhow::Result<bool> {
        let token_hash = hash_member_token(token);
        let touched = sqlx::query(
            "UPDATE member_tokens
             SET last_used_at = now()
             WHERE token_hash = $1
               AND revoked_at IS NULL
               AND (last_used_at IS NULL OR last_used_at < now() - make_interval(secs => 60))",
        )
        .bind(&token_hash)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;

        let authenticated = if touched {
            true
        } else {
            let (active,): (bool,) = sqlx::query_as(
                "SELECT EXISTS (
                     SELECT 1 FROM member_tokens
                     WHERE token_hash = $1 AND revoked_at IS NULL
                 )",
            )
            .bind(token_hash)
            .fetch_one(&self.pool)
            .await?;
            active
        };
        Ok(authenticated)
    }

    /// Every registered repo with its sync position: last synced commit,
    /// the in-flight fetch job, if any, and its current Shard, if any.
    pub async fn admin_status(&self) -> anyhow::Result<Vec<RepoSyncStatus>> {
        let rows = sqlx::query_as(
            // Plain joins suffice: jobs_one_in_flight_per_repo_kind
            // guarantees at most one non-done job per (repo, kind).
            "SELECT r.slug, f.base_url AS forge, r.visibility, r.discovery_state,
                    r.last_synced_commit,
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

    /// Refresh queue-depth gauges from Postgres, the source of truth
    /// shared by API-only and worker-only processes.
    pub async fn refresh_job_queue_depths(&self) -> anyhow::Result<()> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT kind, count(*)::bigint
             FROM jobs
             WHERE state <> 'done'
             GROUP BY kind",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut depths = [0_u64; JobKind::ALL.len()];
        for (raw_kind, depth) in rows {
            let kind = JobKind::from_database(&raw_kind)
                .with_context(|| format!("unknown job kind in queue: {raw_kind}"))?;
            let depth = u64::try_from(depth).context("job queue depth cannot be negative")?;
            let index = JobKind::ALL
                .iter()
                .position(|candidate| *candidate == kind)
                .expect("every parsed job kind is in JobKind::ALL");
            depths[index] = depth;
        }
        for (kind, depth) in JobKind::ALL.into_iter().zip(depths) {
            self.metrics.set_queue_depth(kind, depth);
        }
        Ok(())
    }

    /// Start a typed duration/outcome observation for worker execution.
    pub fn start_job(&self, kind: JobKind) -> JobTimer {
        self.metrics.start_job(kind)
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
            JobKind::Fetch,
            "",
            "r.slug, r.fetch_depth, f.kind AS forge_kind, f.base_url, f.token_env",
        ));
        let row: Option<LeasedFetch> = sqlx::query_as(sql)
            .bind(lease.as_secs_f64())
            .fetch_optional(&self.pool)
            .await?;
        if let Some(job) = &row {
            self.metrics
                .observe_claim_latency(JobKind::Fetch, job.claim_latency_seconds);
        }
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
        let (discovery_state,): (String,) =
            sqlx::query_as("SELECT discovery_state FROM repos WHERE id = $1 FOR UPDATE")
                .bind(job.repo_id)
                .fetch_one(&mut *tx)
                .await?;
        if discovery_state != "included" {
            settle_leased_job(&mut tx, job.job_id, &job.lease_token, true).await?;
            tx.commit().await?;
            return Ok(false);
        }
        if !settle_leased_job(&mut tx, job.job_id, &job.lease_token, true).await? {
            return Ok(false);
        }
        // Every synced commit wants indexing. The index job carries no
        // payload: a worker reads the repo's sync position at claim time.
        // If a job is already in flight this no-ops — a queued job will
        // pick the new commit up at claim, and a leased one is re-queued
        // by complete_index when it notices the repo moved on.
        enqueue_job_unless_in_flight(&mut tx, JobKind::Index, job.repo_id).await?;
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

    /// Return a fetch claim to the immediately-due queue without
    /// recording a failed attempt. The lease token fences shutdown from
    /// releasing a job that a healthy worker has already reclaimed.
    pub async fn release_fetch(&self, job: &LeasedFetch) -> anyhow::Result<bool> {
        self.release_leased_job(job.job_id, &job.lease_token).await
    }

    /// Extend a fetch job's lease so work that outlives the base lease —
    /// a cold full-history clone — is not reclaimed mid-run. Lease-fenced
    /// like completion: `false` means the job was reclaimed and this
    /// worker's token is dead, so it should abandon the run.
    pub async fn renew_fetch(
        &self,
        job: &LeasedFetch,
        lease: std::time::Duration,
    ) -> anyhow::Result<bool> {
        self.renew_leased_job(job.job_id, &job.lease_token, lease)
            .await
    }

    /// Extend an index job's lease, exactly like [`Self::renew_fetch`].
    pub async fn renew_index(
        &self,
        job: &LeasedIndex,
        lease: std::time::Duration,
    ) -> anyhow::Result<bool> {
        self.renew_leased_job(job.job_id, &job.lease_token, lease)
            .await
    }

    /// Push a leased job's deadline out by `lease` from now, but only
    /// while the caller still holds the lease — after reclamation the old
    /// token matches nothing, so a lapsed worker cannot resurrect its
    /// claim. The new deadline is a new fencing token (the token is the
    /// deadline's text rendering); a successful renewal swaps it into the
    /// job so the eventual settle still matches.
    ///
    /// A renewal that commits but whose response is lost leaves the
    /// worker holding the stale token: its next renewal reads as fenced
    /// and its result is discarded at settle, even though no one took
    /// the job. That fails safe — the job re-runs at lease expiry — and
    /// is accepted; distinguishing it would need a token that survives
    /// the round trip (a fence counter column).
    async fn renew_leased_job(
        &self,
        job_id: i64,
        lease_token: &LeaseToken,
        lease: std::time::Duration,
    ) -> anyhow::Result<bool> {
        let renewed: Option<(String,)> = sqlx::query_as(
            "UPDATE jobs
             SET lease_until = now() + make_interval(secs => $2)
             WHERE id = $1 AND state = 'leased' AND lease_until::text = $3
             RETURNING lease_until::text",
        )
        .bind(job_id)
        .bind(lease.as_secs_f64())
        .bind(lease_token.current())
        .fetch_optional(&self.pool)
        .await?;
        match renewed {
            Some((token,)) => {
                lease_token.replace(token);
                Ok(true)
            }
            None => Ok(false),
        }
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
                 SELECT r.id,
                        greatest(extract(epoch FROM now() - r.next_poll_at), 0)::float8
                            AS poll_lag_seconds
                 FROM repos r
                 WHERE r.discovery_state = 'included'
                   AND r.last_synced_commit IS NOT NULL
                   AND r.next_poll_at <= now()
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
                       f.kind AS forge_kind, f.base_url, f.token_env, r.fetch_depth,
                       r.poll_interval_seconds,
                       r.last_synced_commit AS synced_commit, f.rate_budget,
                       due.poll_lag_seconds",
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
        let included: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM repos WHERE id = $1 AND discovery_state = 'included' FOR UPDATE",
        )
        .bind(repo_id)
        .fetch_optional(&mut *tx)
        .await?;
        let queued = if included.is_some() {
            enqueue_job_unless_in_flight(&mut tx, JobKind::Fetch, repo_id).await?
        } else {
            false
        };
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
            JobKind::Index,
            "AND r.last_synced_commit IS NOT NULL",
            "r.slug, r.last_synced_commit AS commit,
             f.kind AS forge_kind, f.base_url, f.token_env, r.fetch_depth",
        ));
        let row: Option<LeasedIndex> = sqlx::query_as(sql)
            .bind(lease.as_secs_f64())
            .fetch_optional(&self.pool)
            .await?;
        if let Some(job) = &row {
            self.metrics
                .observe_claim_latency(JobKind::Index, job.claim_latency_seconds);
        }
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
        let (current_commit, old_shard_id, discovery_state): (Option<String>, Option<i64>, String) =
            sqlx::query_as(
                "SELECT last_synced_commit, current_shard_id, discovery_state
             FROM repos
             WHERE id = $1
             FOR UPDATE",
            )
            .bind(job.repo_id)
            .fetch_one(&mut *tx)
            .await?;
        if discovery_state != "included" {
            settle_leased_job(&mut tx, job.job_id, &job.lease_token, true).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let up_to_date = current_commit.as_deref() == Some(job.commit.as_str());
        let existing_state: Option<ShardState> = sqlx::query_scalar(
            "SELECT state FROM shards
             WHERE repo_id = $1 AND revision = $2
             FOR UPDATE",
        )
        .bind(job.repo_id)
        .bind(shard.revision)
        .fetch_optional(&mut *tx)
        .await?;
        if !shard_accepts_publication(existing_state) {
            defer_leased_index_for_reclamation(&mut tx, job).await?;
            tx.commit().await?;
            return Ok(false);
        }
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
             WHERE r.discovery_state = 'included'
               AND r.last_synced_commit IS NOT NULL
               AND right(s.revision, length($1)) <> $1
             ORDER BY r.id",
        )
        .bind(current_suffix)
        .fetch_all(&mut *tx)
        .await?;
        let mut queued = 0;
        for (repo_id,) in outdated {
            if enqueue_job_unless_in_flight(&mut tx, JobKind::Index, repo_id).await? {
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
        let rows = sqlx::query_as(SUPERSEDED_SHARDS_PAST_GRACE_QUERY)
            .bind(grace.as_secs_f64())
            .bind(ShardState::Reclaiming.as_str())
            .bind(ShardState::Published.as_str())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Claim a superseded Shard's row for reclamation by moving it to
    /// [`ShardState::Reclaiming`], but
    /// only while no repo points at it. Revisions are deterministic per
    /// `(commit, pass, schema)`, so a force-push back to a Shard's commit
    /// can republish that exact revision and re-point a repo at this very
    /// row between the GC sweep's eligibility scan and this claim; the
    /// `NOT EXISTS` guard makes that case a no-op instead of a foreign-key
    /// error (which would wedge the sweep, retrying forever). Returns
    /// whether the row was claimed — `false` means it became current
    /// again and its object-storage segments must be left alone.
    pub async fn delete_superseded_shard(&self, shard_id: i64) -> anyhow::Result<bool> {
        let claimed = sqlx::query(
            "UPDATE shards s SET state = $2
             WHERE s.id = $1
               AND s.state IN ($3, $2)
               AND NOT EXISTS (SELECT 1 FROM repos r WHERE r.current_shard_id = s.id)",
        )
        .bind(shard_id)
        .bind(ShardState::Reclaiming.as_str())
        .bind(ShardState::Published.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        Ok(claimed)
    }

    /// Reap a reclaimed Shard row after its manifest and segments are
    /// gone. Repeating both guards prevents a stale GC worker from
    /// removing a row that is no longer its responsibility.
    pub async fn finish_shard_reclamation(&self, shard_id: i64) -> anyhow::Result<bool> {
        let reaped = sqlx::query(
            "DELETE FROM shards s
             WHERE s.id = $1
               AND s.state = $2
               AND NOT EXISTS (SELECT 1 FROM repos r WHERE r.current_shard_id = s.id)",
        )
        .bind(shard_id)
        .bind(ShardState::Reclaiming.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        Ok(reaped)
    }

    /// Remove terminal job rows finished longer ago than `retention`.
    /// Nothing reads a terminal row (`yg admin status` joins only
    /// non-done jobs), so without retention they accumulate forever for
    /// no one. Rows still in flight (or re-queued) carry no
    /// `finished_at` and are never touched. Returns how many rows were
    /// removed.
    pub async fn delete_terminal_jobs_past_retention(
        &self,
        retention: std::time::Duration,
    ) -> anyhow::Result<u64> {
        let deleted = sqlx::query(
            "DELETE FROM jobs
             WHERE state = 'done'
               AND finished_at < now() - make_interval(secs => $1)",
        )
        .bind(retention.as_secs_f64())
        .execute(&self.pool)
        .await?
        .rows_affected();
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

    /// Return an index claim to the immediately-due queue without
    /// recording a failed attempt. See [`Self::release_fetch`].
    pub async fn release_index(&self, job: &LeasedIndex) -> anyhow::Result<bool> {
        self.release_leased_job(job.job_id, &job.lease_token).await
    }

    async fn release_leased_job(
        &self,
        job_id: i64,
        lease_token: &LeaseToken,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let released = settle_leased_job(&mut tx, job_id, lease_token, false).await?;
        tx.commit().await?;
        Ok(released)
    }

    /// Re-queue a leased job with exponential backoff (30s doubling per
    /// attempt, capped at an hour), keeping the error for `yg admin
    /// status`. Lease-fenced like completion: `false` means the lease had
    /// lapsed and the failure was discarded.
    async fn fail_leased_job(
        &self,
        job_id: i64,
        lease_token: &LeaseToken,
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
        .bind(lease_token.current())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1;
        Ok(applied)
    }

    /// How many repos are indexed into the Knowledge Graph: those whose
    /// current-Shard pointer is set.
    pub async fn indexed_repo_count(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM repos
             WHERE discovery_state = 'included' AND current_shard_id IS NOT NULL",
        )
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
             WHERE r.qualifier = $1 AND r.discovery_state = 'included'",
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
             WHERE r.discovery_state = 'included'
               AND right(s.revision, length($1)) = $1
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

fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)
        .map_err(|e| anyhow::anyhow!("generating secure random bytes: {e}"))?;
    Ok(hex_bytes(&buf))
}

fn hash_member_token(token: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(token.as_bytes());
    format!("sha256:{}", hex_bytes(&digest))
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

const SUPERSEDED_SHARDS_PAST_GRACE_QUERY: &str =
    "SELECT s.id AS shard_id, s.repo_id, s.revision, s.state FROM shards s
     WHERE NOT EXISTS (SELECT 1 FROM repos r WHERE r.current_shard_id = s.id)
       AND (s.state = $2
            OR (s.state = $3
                AND coalesce(s.superseded_at, s.published_at)
                    < now() - make_interval(secs => $1)))
     ORDER BY s.id";

/// A plan-equivalent rendering of the superseded-Shard eligibility query for
/// e2e `EXPLAIN` tests, which substitute its sole `$1` grace parameter. The
/// production query binds the state vocabulary from [`ShardState`]; the unit
/// drift guard pins these test-only literals to that vocabulary.
pub const SUPERSEDED_SHARDS_PAST_GRACE_SQL: &str =
    "SELECT s.id AS shard_id, s.repo_id, s.revision, s.state FROM shards s
     WHERE NOT EXISTS (SELECT 1 FROM repos r WHERE r.current_shard_id = s.id)
       AND (s.state = 'reclaiming'
            OR (s.state = 'published'
                AND coalesce(s.superseded_at, s.published_at)
                    < now() - make_interval(secs => $1)))
     ORDER BY s.id";

/// The one lease-claim query, shared by every job kind so the claim
/// protocol can't drift between kinds. `$1` is the lease in seconds.
/// `state <> 'done'` looks implied by the queued-or-expired OR (done
/// rows have lease_until NULL), but the planner needs the partial index
/// predicate verbatim to use jobs_claim_scan. The lease_until::text
/// rendering is the fencing token settle/fail compare against. Kind,
/// an extra due-predicate (over jobs `j` and repos `r`), and the extra
/// RETURNING columns are the only legitimate variation. Public so the
/// e2e plan tests can EXPLAIN the exact production query; the fragments
/// are `&'static str` so only compile-time constants — never runtime
/// input — can reach the assembled SQL.
pub fn claim_due_sql(
    kind: JobKind,
    extra_due_predicate: &'static str,
    returning: &'static str,
) -> String {
    let kind = kind.as_str();
    format!(
        "WITH due AS (
             SELECT j.id,
                    greatest(extract(epoch FROM now() - CASE
                        WHEN j.state = 'leased' THEN j.lease_until
                        ELSE j.run_after
                    END), 0)::float8 AS claim_latency_seconds
             FROM jobs j JOIN repos r ON r.id = j.repo_id
             WHERE j.kind = '{kind}' AND j.state <> 'done' AND j.run_after <= now()
               AND r.discovery_state = 'included'
               AND (j.state = 'queued' OR j.lease_until < now())
               {extra_due_predicate}
             ORDER BY j.run_after
             LIMIT 1
             FOR UPDATE OF j SKIP LOCKED
         )
         UPDATE jobs j
         SET state = 'leased', lease_until = now() + make_interval(secs => $1)
         FROM due, repos r JOIN forges f ON f.id = r.forge_id
         WHERE j.id = due.id AND r.id = j.repo_id
         RETURNING j.id AS job_id, j.repo_id, j.attempts, {returning},
                   j.lease_until::text AS lease_token,
                   due.claim_latency_seconds"
    )
}

async fn rules_for_forge(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    forge_id: i64,
) -> anyhow::Result<Vec<RuleRow>> {
    let rules = sqlx::query_as(
        "SELECT pattern, action, applies_to_private
         FROM rules
         WHERE forge_id = $1
         ORDER BY length(pattern) DESC, created_at DESC, id DESC",
    )
    .bind(forge_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rules)
}

async fn lock_forge_for_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    forge_id: i64,
) -> anyhow::Result<()> {
    let locked: Option<(i64,)> = sqlx::query_as("SELECT id FROM forges WHERE id = $1 FOR UPDATE")
        .bind(forge_id)
        .fetch_optional(&mut **tx)
        .await?;
    if locked.is_none() {
        anyhow::bail!("forge {forge_id} does not exist");
    }
    Ok(())
}

async fn put_rule_newest(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    forge_id: i64,
    pattern: &str,
    action: RuleAction,
    applies_to_private: bool,
) -> anyhow::Result<bool> {
    let (created,): (bool,) = sqlx::query_as(
        "INSERT INTO rules (forge_id, pattern, action, applies_to_private)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (forge_id, pattern, action) DO UPDATE
         SET applies_to_private = excluded.applies_to_private,
             created_at = now()
         RETURNING (xmax = 0)",
    )
    .bind(forge_id)
    .bind(pattern)
    .bind(action.as_str())
    .bind(applies_to_private)
    .fetch_one(&mut **tx)
    .await?;
    Ok(created)
}

async fn remove_repo_from_indexing(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    repo_id: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE shards
         SET superseded_at = now()
         WHERE id = (SELECT current_shard_id FROM repos WHERE id = $1)",
    )
    .bind(repo_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE repos SET current_shard_id = NULL WHERE id = $1")
        .bind(repo_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "UPDATE jobs
         SET state = 'done', lease_until = NULL, finished_at = now()
         WHERE repo_id = $1 AND state <> 'done'",
    )
    .bind(repo_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Rule precedence is deterministic: the most specific matching glob
/// (longest pattern) wins, and the newest rule wins ties. Private repos
/// only consider rules with `applies_to_private`, making private indexing
/// an explicit opt-in path per ADR 0001.
fn discovery_state_for(slug: &str, visibility: RepoVisibility, rules: &[RuleRow]) -> &'static str {
    for rule in rules {
        if visibility == RepoVisibility::Private && !rule.applies_to_private {
            continue;
        }
        if glob_matches(&rule.pattern, slug) {
            return if rule.action == "include" {
                "included"
            } else {
                "excluded"
            };
        }
    }
    match visibility {
        RepoVisibility::Private => "discovered",
        RepoVisibility::Public | RepoVisibility::Internal => "included",
    }
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut p, mut t) = (0, 0);
    let mut star = None;
    let mut retry_text = 0;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry_text = t;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            retry_text += 1;
            t = retry_text;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn shard_accepts_publication(state: Option<ShardState>) -> bool {
    !matches!(state, Some(ShardState::Reclaiming))
}

const RECLAMATION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

async fn defer_leased_index_for_reclamation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    job: &LeasedIndex,
) -> anyhow::Result<bool> {
    let deferred = settle_leased_job(tx, job.job_id, &job.lease_token, false).await?;
    if deferred {
        sqlx::query(
            "UPDATE jobs SET run_after = now() + make_interval(secs => $2)
             WHERE id = $1 AND state = 'queued'",
        )
        .bind(job.job_id)
        .bind(RECLAMATION_RETRY_DELAY.as_secs_f64())
        .execute(&mut **tx)
        .await?;
    }
    Ok(deferred)
}

/// Queue a job for the repo unless one of its kind is already in flight
/// — jobs_one_in_flight_per_repo_kind dedups in-flight work. Returns
/// whether a new job was queued.
async fn enqueue_job_unless_in_flight(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: JobKind,
    repo_id: i64,
) -> anyhow::Result<bool> {
    let queued = sqlx::query(
        "INSERT INTO jobs (kind, repo_id) VALUES ($1, $2)
         ON CONFLICT (repo_id, kind) WHERE state <> 'done' DO NOTHING",
    )
    .bind(kind.as_str())
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
    lease_token: &LeaseToken,
    done: bool,
) -> anyhow::Result<bool> {
    let claimed = sqlx::query(
        "UPDATE jobs
         SET state = CASE WHEN $3 THEN 'done' ELSE 'queued' END,
             attempts = CASE WHEN $3 THEN attempts ELSE 0 END,
             last_error = CASE WHEN $3 THEN last_error ELSE NULL END,
             finished_at = CASE WHEN $3 THEN now() END,
             lease_until = NULL, run_after = now()
         WHERE id = $1 AND state = 'leased' AND lease_until::text = $2",
    )
    .bind(job_id)
    .bind(lease_token.current())
    .bind(done)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    Ok(claimed)
}

#[cfg(test)]
mod tests {
    use super::{
        JobKind, REPO_QUALIFIER_SQL, RepoVisibility, RuleRow, SUPERSEDED_SHARDS_PAST_GRACE_QUERY,
        SUPERSEDED_SHARDS_PAST_GRACE_SQL, ShardState, discovery_state_for, glob_matches,
        member_token_id_is_valid, repo_qualifier, shard_accepts_publication,
    };

    const HYGIENE_MIGRATION: &str = include_str!("../migrations/0011_job_queue_hygiene.sql");
    const SHARD_STATE_MIGRATION: &str = include_str!("../migrations/0013_shard_reclaiming.sql");

    /// One direction of the single-sourced qualifier grammar: the SQL
    /// rendering the migration re-derives with is the constant the e2e
    /// suite pins to `repo_qualifier` — so neither the migration nor the
    /// Rust function can drift alone.
    #[test]
    fn the_qualifier_migration_uses_the_pinned_sql_rendering() {
        assert!(
            HYGIENE_MIGRATION.contains(REPO_QUALIFIER_SQL),
            "migration 0011 must re-derive qualifiers with REPO_QUALIFIER_SQL verbatim"
        );
    }

    /// The kind CHECK constraint mirrors [`JobKind`]: the clause the
    /// migration installs is exactly the one this vocabulary renders.
    #[test]
    fn the_kind_constraint_mirrors_the_rust_vocabulary() {
        let clause = format!(
            "kind IN ({})",
            JobKind::ALL
                .map(|kind| format!("'{}'", kind.as_str()))
                .join(", ")
        );
        assert!(
            HYGIENE_MIGRATION.contains(&clause),
            "migration 0011 must constrain kind to {clause}"
        );
    }

    #[test]
    fn the_shard_state_constraint_mirrors_the_rust_vocabulary() {
        let clause = format!(
            "state IN ({})",
            ShardState::ALL
                .map(|state| format!("'{}'", state.as_str()))
                .join(", ")
        );
        assert!(
            SHARD_STATE_MIGRATION.contains(&clause),
            "migration 0013 must constrain Shard state to {clause}"
        );
        let rendered_query = SUPERSEDED_SHARDS_PAST_GRACE_QUERY
            .replace("$2", &format!("'{}'", ShardState::Reclaiming.as_str()))
            .replace("$3", &format!("'{}'", ShardState::Published.as_str()));
        assert_eq!(
            rendered_query, SUPERSEDED_SHARDS_PAST_GRACE_SQL,
            "the e2e query fixture must exactly render the production query's state bindings"
        );
    }

    #[tokio::test]
    async fn republish_racing_reclamation_never_advances_the_current_pointer() {
        use std::sync::Arc;

        struct SeamFence(tokio::sync::OwnedMutexGuard<()>);

        impl super::ShardOperationFence for SeamFence {
            async fn release(self) {
                drop(self.0);
            }
        }

        #[derive(Clone, Copy)]
        struct SeamState {
            row: Option<ShardState>,
            manifest: bool,
            segment: bool,
            current: bool,
        }

        let operation = Arc::new(tokio::sync::Mutex::new(()));
        let state = Arc::new(tokio::sync::Mutex::new(SeamState {
            row: Some(ShardState::Published),
            manifest: true,
            segment: true,
            current: false,
        }));
        let gc_claimed = Arc::new(tokio::sync::Barrier::new(2));
        let allow_gc_finish = Arc::new(tokio::sync::Barrier::new(2));

        let gc = {
            let operation = operation.clone();
            let state = state.clone();
            let gc_claimed = gc_claimed.clone();
            let allow_gc_finish = allow_gc_finish.clone();
            tokio::spawn(async move {
                let fence = SeamFence(operation.lock_owned().await);
                super::finish_shard_operation(fence, async {
                    state.lock().await.row = Some(ShardState::Reclaiming);
                    gc_claimed.wait().await;
                    allow_gc_finish.wait().await;
                    let mut state = state.lock().await;
                    state.manifest = false;
                    state.segment = false;
                    assert!(!state.current);
                    state.row = None;
                })
                .await;
            })
        };
        gc_claimed.wait().await;
        assert!(
            operation.try_lock().is_err(),
            "GC must retain the production fence while its transition is paused"
        );
        let publisher = {
            let operation = operation.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let fence = SeamFence(operation.lock_owned().await);
                super::finish_shard_operation(fence, async {
                    let mut state = state.lock().await;
                    assert!(shard_accepts_publication(state.row));
                    state.segment = true;
                    state.manifest = true;
                    state.row = Some(ShardState::Published);
                    state.current = true;
                })
                .await;
            })
        };

        allow_gc_finish.wait().await;
        gc.await.unwrap();
        publisher.await.unwrap();
        let state = state.lock().await;
        assert_eq!(state.row, Some(ShardState::Published));
        assert!(state.current && state.manifest && state.segment);
    }

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

    #[test]
    fn discovery_globs_match_without_recursive_backtracking() {
        assert!(glob_matches("acme/*", "acme/widgets"));
        assert!(glob_matches("acme/private-?", "acme/private-a"));
        assert!(!glob_matches("acme/private-?", "acme/private-ab"));
        assert!(!glob_matches("acme/*/api", "acme/widgets"));
        assert!(glob_matches("********widgets", "acme/widgets"));
    }

    #[test]
    fn member_token_ids_are_strict_url_path_components() {
        assert!(member_token_id_is_valid("mtok_0123456789abcdefABCDEF01"));
        for id in [
            "mtok_0123456789abcdefABCDEF0",
            "mtok_0123456789abcdefABCDEF012",
            "token_0123456789abcdefABCDEF01",
            "mtok_0123456789abcdefABCDEG01",
            "mtok_0123456789abcdefABCDE/01",
            "mtok_0123456789abcdefABCDE?01",
        ] {
            assert!(!member_token_id_is_valid(id), "{id:?} must be rejected");
        }
    }

    #[test]
    fn longest_private_applicable_rule_wins_discovery_state() {
        let rules = vec![
            RuleRow {
                pattern: "acme/*".into(),
                action: "include".into(),
                applies_to_private: false,
            },
            RuleRow {
                pattern: "acme/private-*".into(),
                action: "include".into(),
                applies_to_private: true,
            },
        ];
        assert_eq!(
            discovery_state_for("acme/private-widgets", RepoVisibility::Private, &rules),
            "included"
        );
        assert_eq!(
            discovery_state_for("acme/secret", RepoVisibility::Private, &rules),
            "discovered",
            "private repos ignore rules not explicitly marked private"
        );
    }
}
