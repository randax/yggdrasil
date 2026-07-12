//! The Sync worker's loops: fetch, poll, and forge-org discovery.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use yg_control::ControlPlane;

use crate::forge::{Forge, ForgeRegistry, github::discovery_client};
use crate::git::{GitFetcher, forge_token, lock_mirror, remote_head_commit};
use crate::lease::with_lease_heartbeat;
use crate::locator::join_clone_url;
use crate::rate::{RATE_LIMIT_COOLDOWN, TokenBucket};

/// How long a worker may hold a fetch job before a crashed run becomes
/// claimable again. Generous: a cold full-history clone of a large repo.
const FETCH_LEASE: Duration = Duration::from_secs(15 * 60);
/// When a poll observes a moved head but a fetch is already queued or
/// leased, retry soon: that in-flight fetch may have read the previous
/// head before this poll observed the push. Capped so large default poll
/// intervals do not hide the detected move for minutes.
const FETCH_IN_FLIGHT_REPOLL_MAX: Duration = Duration::from_secs(30);

/// A Sync worker: drains the fetch queue, mirroring repos into the
/// worker-local git cache and recording each repo's synced commit.
pub struct SyncWorker {
    control: ControlPlane,
    fetcher: GitFetcher,
    /// The forge adapters this worker dispatches through — the built-ins
    /// by default, injectable so tests register doubles.
    registry: ForgeRegistry,
    discovery_client: reqwest::Client,
    /// Per-forge rate budgets, keyed by forge id. Created on first poll
    /// of a forge and kept for the worker's life; the poll loop spends a
    /// token per conditional request and backs the forge off on a
    /// rate-limit signal. In-process: one worker's view of its own
    /// request rate (the per-repo interval smooths the fleet's).
    poll_buckets: Mutex<HashMap<i64, TokenBucket>>,
}

impl SyncWorker {
    pub fn new(control: ControlPlane, git_cache: impl Into<PathBuf>) -> Self {
        Self::with_registry(control, git_cache, ForgeRegistry::builtin())
    }

    /// A worker dispatching through `registry` instead of the built-in
    /// adapters — how a test registers a forge double.
    pub fn with_registry(
        control: ControlPlane,
        git_cache: impl Into<PathBuf>,
        registry: ForgeRegistry,
    ) -> Self {
        Self {
            control,
            fetcher: GitFetcher::new(git_cache),
            registry,
            discovery_client: discovery_client(),
            poll_buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Claim and run one due job. Returns whether there was work. A
    /// failed fetch is recorded (with backoff) rather than returned as an
    /// error — `Err` means the control plane itself is unreachable.
    pub async fn run_once(&self) -> anyhow::Result<bool> {
        let Some(job) = self.control.claim_due_fetch(FETCH_LEASE).await? else {
            return Ok(false);
        };
        let clone_url = join_clone_url(&job.base_url, &job.slug);
        let forge = self.registry.for_kind(&job.forge_kind);
        let auth = forge_token(job.token_env.as_deref(), &clone_url).map(|t| forge.git_auth(t));
        // A lock failure (cache dir on a dead disk) is a failed fetch,
        // not a dead worker: record it and let backoff retry.
        let work = async {
            let _serialize_same_mirror =
                lock_mirror(self.fetcher.cache_dir(), job.repo_id, FETCH_LEASE).await?;
            self.fetcher
                .sync(job.repo_id, &clone_url, auth.as_ref(), job.fetch_depth)
                .await
        };
        // A cold clone of a large repo outlives the base lease; the
        // heartbeat keeps the job ours for as long as the work is alive.
        let renew = async || self.control.renew_fetch(&job, FETCH_LEASE).await;
        let synced = with_lease_heartbeat(FETCH_LEASE, renew, work).await;
        match synced {
            Ok(commit) => {
                if self.control.complete_fetch(&job, &commit).await? {
                    tracing::info!(slug = %job.slug, %commit, "synced");
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-fetch; result discarded");
                }
            }
            Err(e) => {
                let error = format!("{e:#}");
                if self.control.fail_fetch(&job, &error).await? {
                    tracing::warn!(slug = %job.slug, attempt = job.attempts + 1, error, "fetch failed");
                } else {
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-fetch; failure discarded");
                }
            }
        }
        Ok(true)
    }

    /// Claim one due forge org and reconcile its repositories through
    /// the forge's discovery adapter; forge kinds without one are
    /// skipped with a warning until their adapters arrive. Returns
    /// whether there was a due discovery claim.
    pub async fn discover_once(&self, cfg: &DiscoveryConfig) -> anyhow::Result<bool> {
        let Some(due) = self.control.claim_due_discovery(cfg.interval).await? else {
            return Ok(false);
        };
        let discovery = self
            .registry
            .by_kind(&due.forge_kind)
            .and_then(Forge::discovery);
        let Some(discovery) = discovery else {
            tracing::warn!(
                forge_kind = %due.forge_kind,
                org = %due.org_slug,
                "forge discovery adapter is not implemented"
            );
            return Ok(true);
        };
        let Some(api_root) = due.api_root.as_deref() else {
            tracing::warn!(
                forge_kind = %due.forge_kind,
                org = %due.org_slug,
                "the forge record has no API root; re-add the forge to backfill it"
            );
            return Ok(true);
        };
        let token = due.token_env.as_deref().and_then(|var| {
            std::env::var(var)
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
        });
        let repos = match discovery
            .list_org_repos(
                &self.discovery_client,
                api_root,
                &due.org_slug,
                token.as_deref(),
            )
            .await
        {
            Ok(repos) => repos,
            Err(e) => {
                tracing::warn!(
                    org = %due.org_slug,
                    error = format!("{e:#}"),
                    "forge discovery failed; will retry on the next discovery interval"
                );
                return Ok(true);
            }
        };
        let discovered: Vec<_> = repos
            .iter()
            .map(|repo| yg_control::DiscoveredRepo {
                slug: &repo.slug,
                visibility: repo.visibility,
                fetch_depth: None,
            })
            .collect();
        let queued = match self
            .control
            .discover_forge_repos(due.org_id, &discovered)
            .await
        {
            Ok(queued) => queued,
            Err(e) if e.downcast_ref::<yg_control::QualifierConflict>().is_some() => {
                tracing::warn!(
                    org = %due.org_slug,
                    error = format!("{e:#}"),
                    "forge discovery found a repo qualifier conflict; skipping this discovery pass"
                );
                return Ok(true);
            }
            Err(e) => return Err(e),
        };
        tracing::info!(
            org = %due.org_slug,
            repos = discovered.len(),
            queued,
            "forge discovery reconciled repositories"
        );
        Ok(true)
    }

    /// Claim one due repo and compare its default-branch head against the
    /// synced position with a single cheap conditional request (`git
    /// ls-remote`). A moved head enqueues a fetch — which re-syncs and
    /// re-indexes the repo — while an unchanged head costs only that one
    /// request, transferring no objects (RFC 0001 §3, issue #9). Returns
    /// whether a repo was due.
    ///
    /// The conditional request is spent only within the forge's rate
    /// budget: a claimed repo that would put the forge over budget (or
    /// whose forge is cooling down from a rate-limit signal) is
    /// rescheduled for when a request frees up, its head left unchecked
    /// this cycle.
    ///
    /// Best-effort like the fetch loop: a failed conditional request is
    /// logged and the repo polls again later, rather than returned as an
    /// error — `Err` means the control plane itself is unreachable.
    ///
    /// Returns whether the loop should keep claiming immediately: `true`
    /// when a head was actually checked, `false` when nothing was due or
    /// the only outcome was deferring an over-budget / cooling-down forge.
    /// Returning `false` on a pure defer is what stops the loop from
    /// hot-spinning through claim+defer for every due repo when a forge is
    /// over budget — the caller sleeps, and the bucket refills meanwhile.
    pub async fn poll_once(&self, cfg: &PollConfig) -> anyhow::Result<bool> {
        let Some(due) = self
            .control
            .claim_due_poll(cfg.default_interval, cfg.jitter_fraction)
            .await?
        else {
            return Ok(false);
        };
        // Spend a rate-budget token; over budget, reschedule the repo for
        // when one frees up and back off (no head check this cycle).
        if let Err(retry) = self.take_poll_token(due.forge_id, due.rate_budget) {
            self.control.defer_poll(due.repo_id, retry).await?;
            return Ok(false);
        }
        let clone_url = join_clone_url(&due.base_url, &due.slug);
        let forge = self.registry.for_kind(&due.forge_kind);
        let auth = forge_token(due.token_env.as_deref(), &clone_url).map(|t| forge.git_auth(t));
        match remote_head_commit(&clone_url, auth.as_ref()).await {
            Ok(Some(head)) if head != due.synced_commit => {
                if self.control.request_fetch(due.repo_id).await? {
                    tracing::info!(slug = %due.slug, %head, "head moved; fetch queued");
                } else {
                    self.control
                        .defer_poll(due.repo_id, in_flight_fetch_repoll(&due, cfg))
                        .await?;
                    tracing::info!(
                        slug = %due.slug,
                        %head,
                        "head moved but fetch is already pending; poll retry scheduled"
                    );
                }
            }
            // Unchanged, or an unborn/hidden head: nothing to fetch.
            Ok(_) => {}
            Err(e) => {
                let detail = format!("{e:#}");
                if forge.is_rate_limit(&detail) {
                    // The forge is pushing back: cool the whole forge down,
                    // retry this repo past the cooldown, and back off.
                    let retry = self.cool_forge_down(due.forge_id, due.rate_budget);
                    self.control.defer_poll(due.repo_id, retry).await?;
                    tracing::warn!(slug = %due.slug, "forge rate-limited the poll; backing the forge off");
                    return Ok(false);
                }
                tracing::warn!(slug = %due.slug, error = detail, "poll failed; will retry next interval");
            }
        }
        Ok(true)
    }

    /// Spend one of this forge's rate-budget tokens. `Err(retry_after)`
    /// when none is available — the forge is over budget or cooling down
    /// — so the caller can reschedule the repo for `retry_after` out.
    fn take_poll_token(&self, forge_id: i64, rate_budget: i32) -> Result<(), Duration> {
        let now = Instant::now();
        let mut buckets = self.poll_buckets.lock().expect("poll bucket map poisoned");
        let bucket = buckets
            .entry(forge_id)
            .or_insert_with(|| TokenBucket::per_minute(rate_budget, now));
        bucket.update_rate(rate_budget, now);
        if bucket.try_take(now) {
            Ok(())
        } else {
            Err(bucket.retry_after(now))
        }
    }

    /// Cool a forge down after it signalled a rate limit, returning when
    /// its next poll should be attempted (past the cooldown).
    fn cool_forge_down(&self, forge_id: i64, rate_budget: i32) -> Duration {
        let now = Instant::now();
        let mut buckets = self.poll_buckets.lock().expect("poll bucket map poisoned");
        let bucket = buckets
            .entry(forge_id)
            .or_insert_with(|| TokenBucket::per_minute(rate_budget, now));
        bucket.update_rate(rate_budget, now);
        bucket.cooldown(now + RATE_LIMIT_COOLDOWN);
        bucket.retry_after(now)
    }
}

/// How the poll loop is paced: the default interval between a repo's
/// default-branch head checks, and the jitter spread (a fraction of the
/// interval) that keeps a forge's repos from polling in lockstep. A
/// per-repo `poll_interval_seconds` override wins over the default; the
/// jitter applies on top of either.
#[derive(Debug, Clone, Copy)]
pub struct PollConfig {
    pub default_interval: Duration,
    pub jitter_fraction: f64,
}

/// How often connected forge orgs are reconciled. The default is one
/// hour; callers can lower it in tests or dev runs.
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryConfig {
    pub interval: Duration,
}

fn in_flight_fetch_repoll(due: &yg_control::DuePoll, cfg: &PollConfig) -> Duration {
    let repo_interval = due
        .poll_interval_seconds
        .map(|secs| Duration::from_secs(secs as u64))
        .unwrap_or(cfg.default_interval);
    repo_interval
        .min(FETCH_IN_FLIGHT_REPOLL_MAX)
        .max(Duration::from_secs(1))
}
