//! The Sync worker's loops: fetch, poll, and forge-org discovery.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use yg_control::{
    ControlPlane, ForgeBudgetTake, ForgeUrl, JobKind, JobOutcome, PollHeadObservation,
    PollRecordOutcome, PollValidators,
};

use crate::forge::{
    ConditionalRequestAccounting, Forge, ForgeBudgetExhausted, ForgeRateLimit, ForgeRegistry,
    ForgeRequestBudget, ListedRepo, OrgDiscovery, RepoPollOutcome, RepoPoller, RepoSlug,
    github::discovery_client,
};
use crate::git::{GitFetcher, forge_token, lock_mirror, remote_head_commit};
use crate::lease::{LeaseShutdown, with_lease_heartbeat, with_lease_heartbeat_until_shutdown};
use crate::locator::{join_clone_url, normalize_repo_slug};
use crate::metrics::Metrics;
use crate::rate::{RATE_LIMIT_COOLDOWN, TokenBucket};
use crate::shutdown::Shutdown;

/// How long a worker may hold a fetch job before a crashed run becomes
/// claimable again. Generous: a cold full-history clone of a large repo.
const FETCH_LEASE: Duration = Duration::from_secs(15 * 60);
/// When a poll observes a moved head but a fetch is already queued or
/// leased, retry soon: that in-flight fetch may have read the previous
/// head before this poll observed the push. Capped so large default poll
/// intervals do not hide the detected move for minutes.
const FETCH_IN_FLIGHT_REPOLL_MAX: Duration = Duration::from_secs(30);
/// Maximum requests per minute granted to one isolated worker when the shared
/// control plane is unavailable. The aggregate fallback rate still grows with
/// worker count: `N` isolated workers have an opening burst of at most `2 * N`
/// requests and the same sustained refill rate per minute (both scale with a
/// lower configured budget). An arbitrary minute starting with full buckets can
/// therefore approach twice that total. This makes the unavoidable breach
/// during a database partition explicit and tightly bounded.
const FALLBACK_REQUESTS_PER_MINUTE: i32 = 2;

/// A Sync worker: drains the fetch queue, mirroring repos into the
/// worker-local git cache and recording each repo's synced commit.
pub struct SyncWorker {
    control: ControlPlane,
    fetcher: GitFetcher,
    /// The forge adapters this worker dispatches through — the built-ins
    /// by default, injectable so tests register doubles.
    registry: ForgeRegistry,
    discovery_client: reqwest::Client,
    /// Stingy per-forge continuity budgets, used only while the shared
    /// control-plane budget is unreachable. Healthy shared grants shadow-spend
    /// these buckets so a brief outage cannot expose a fresh local burst.
    forge_budgets: ForgeRequestBudgets,
    metrics: Metrics,
}

impl SyncWorker {
    pub fn new(control: ControlPlane, git_cache: impl Into<PathBuf>) -> Self {
        Self::with_metrics(control, git_cache, Metrics::unregistered())
    }

    /// A worker whose poll observations are emitted through `metrics`.
    pub fn with_metrics(
        control: ControlPlane,
        git_cache: impl Into<PathBuf>,
        metrics: Metrics,
    ) -> Self {
        Self::with_registry_and_metrics(control, git_cache, ForgeRegistry::builtin(), metrics)
    }

    /// A worker dispatching through `registry` instead of the built-in
    /// adapters — how a test registers a forge double.
    pub fn with_registry(
        control: ControlPlane,
        git_cache: impl Into<PathBuf>,
        registry: ForgeRegistry,
    ) -> Self {
        Self::with_registry_and_metrics(control, git_cache, registry, Metrics::unregistered())
    }

    /// A worker with both an injected Forge registry and metrics handle.
    pub fn with_registry_and_metrics(
        control: ControlPlane,
        git_cache: impl Into<PathBuf>,
        registry: ForgeRegistry,
        metrics: Metrics,
    ) -> Self {
        Self {
            control,
            fetcher: GitFetcher::new(git_cache),
            registry,
            discovery_client: discovery_client(),
            forge_budgets: ForgeRequestBudgets::default(),
            metrics,
        }
    }

    /// Claim and run one due job. Returns whether there was work. A
    /// failed fetch is recorded (with backoff) rather than returned as an
    /// error — `Err` means the control plane itself is unreachable.
    pub async fn run_once(&self) -> anyhow::Result<bool> {
        self.run_once_with_optional_shutdown(None).await
    }

    /// Claim and run one due job while observing process shutdown. New
    /// claims stop immediately; an active fetch gets until the shared
    /// work cutoff to settle normally, then its lease is returned fresh
    /// to the queue before the work future is dropped.
    pub async fn run_once_with_shutdown(&self, shutdown: Shutdown) -> anyhow::Result<bool> {
        if shutdown.deadline().is_some() {
            return Ok(false);
        }
        self.run_once_with_optional_shutdown(Some(shutdown)).await
    }

    async fn run_once_with_optional_shutdown(
        &self,
        shutdown: Option<Shutdown>,
    ) -> anyhow::Result<bool> {
        let (job, timer) = match claim_due_fetch_with_optional_shutdown(
            shutdown.as_ref(),
            self.control.claim_due_fetch(FETCH_LEASE),
            async |job| self.control.release_fetch(job).await,
            || self.control.start_job(JobKind::Fetch),
        )
        .await?
        {
            ShutdownClaim::Empty => return Ok(false),
            ShutdownClaim::Ready { job, timer } => (job, timer),
            ShutdownClaim::Released { timer } => {
                // The job was released untouched for a healthy retry: no
                // work happened, so no outcome is recorded.
                timer.disarm();
                return Ok(true);
            }
        };
        let clone_url = join_clone_url(job.base_url.as_str(), &job.slug);
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
        let synced = if let Some(shutdown) = shutdown {
            let release = async || self.control.release_fetch(&job).await;
            match with_lease_heartbeat_until_shutdown(
                FETCH_LEASE,
                renew,
                release,
                shutdown.clone(),
                work,
            )
            .await?
            {
                LeaseShutdown::Finished(synced) => synced,
                LeaseShutdown::Released => {
                    timer.finish(JobOutcome::Discarded);
                    return Ok(true);
                }
            }
        } else {
            with_lease_heartbeat(FETCH_LEASE, renew, work).await
        };
        match synced {
            Ok(commit) => {
                if self.control.complete_fetch(&job, &commit).await? {
                    timer.finish(JobOutcome::Success);
                    tracing::info!(slug = %job.slug, %commit, "synced");
                } else {
                    timer.finish(JobOutcome::Discarded);
                    tracing::warn!(slug = %job.slug, "lease lapsed mid-fetch; result discarded");
                }
            }
            Err(e) => {
                let error = format!("{e:#}");
                if self.control.fail_fetch(&job, &error).await? {
                    timer.finish(JobOutcome::Failure);
                    tracing::warn!(slug = %job.slug, attempt = job.attempts + 1, error, "fetch failed");
                } else {
                    timer.finish(JobOutcome::Discarded);
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
        let Some(api_root) = due.api_root.as_ref().map(yg_control::ForgeUrl::as_str) else {
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
        let budget = DiscoveryBudget {
            control: &self.control,
            fallback_budgets: &self.forge_budgets,
            forge_id: due.forge_id,
            rate_budget: due.rate_budget,
        };
        let repos = match list_org_repos_with_budget(
            discovery,
            &self.discovery_client,
            api_root,
            &due.org_slug,
            token.as_deref(),
            &budget,
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
        let repos: Vec<ListedRepo> = repos
            .into_iter()
            .filter_map(|repo| match normalize_repo_slug(&repo.slug) {
                Ok(slug) => Some(ListedRepo {
                    slug,
                    visibility: repo.visibility,
                }),
                Err(error) => {
                    tracing::warn!(
                        org = %due.org_slug,
                        slug = ?repo.slug,
                        error = ?error,
                        "forge discovery rejected an invalid repository slug"
                    );
                    None
                }
            })
            .collect();
        let discovered: Vec<_> = repos
            .iter()
            .map(|repo| yg_control::DiscoveredRepo {
                slug: &repo.slug,
                visibility: repo.visibility,
                fetch_depth: None,
            })
            .collect();
        let queued = self
            .control
            .discover_forge_repos(due.org_id, &discovered)
            .await?;
        tracing::info!(
            org = %due.org_slug,
            repos = discovered.len(),
            queued,
            "forge discovery reconciled repositories"
        );
        Ok(true)
    }

    /// Run one discovery pass, cancelling any in-progress Forge budget wait
    /// or API request as soon as process shutdown begins.
    pub async fn discover_once_with_shutdown(
        &self,
        cfg: &DiscoveryConfig,
        shutdown: Shutdown,
    ) -> anyhow::Result<bool> {
        if shutdown.deadline().is_some() {
            return Ok(false);
        }
        Ok(
            cancel_discovery_on_shutdown(shutdown, self.discover_once(cfg))
                .await?
                .unwrap_or(false),
        )
    }

    /// Claim one due repo and compare its default-branch head against the
    /// synced position with one conditional request. API-backed adapters can
    /// use HTTP validators; generic forges retain `git ls-remote`. A moved
    /// head enqueues a fetch, while an unchanged head transfers no objects
    /// (RFC 0001 §3, issue #9). Returns whether a repo was due.
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
        self.metrics
            .observe_poll_lag(due.base_url.as_str(), due.poll_lag_seconds);
        let forge = self.registry.for_kind(&due.forge_kind);
        if let RepoPollRoute::Http { poller, api_root } =
            repo_poll_route(forge, due.api_root.as_ref())
        {
            return self.poll_repo_api(poller, api_root, &due, cfg).await;
        }
        // Spend a rate-budget token; over budget, reschedule the repo for
        // when one frees up and back off (no head check this cycle).
        if let Err(retry) = self.take_forge_token(due.forge_id, due.rate_budget).await {
            self.control.defer_poll(due.repo_id, retry).await?;
            return Ok(false);
        }
        let clone_url = join_clone_url(due.base_url.as_str(), &due.slug);
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
                    let retry = self.cool_forge_down(due.forge_id, due.rate_budget).await;
                    self.control.defer_poll(due.repo_id, retry).await?;
                    tracing::warn!(slug = %due.slug, "forge rate-limited the poll; backing the forge off");
                    return Ok(false);
                }
                tracing::warn!(slug = %due.slug, error = detail, "poll failed; will retry next interval");
            }
        }
        Ok(true)
    }

    async fn poll_repo_api(
        &self,
        poller: &dyn RepoPoller,
        api_root: &ForgeUrl,
        due: &yg_control::DuePoll,
        cfg: &PollConfig,
    ) -> anyhow::Result<bool> {
        let slug = match RepoSlug::parse(due.slug.clone()) {
            Ok(slug) => slug,
            Err(error) => {
                tracing::warn!(slug = %due.slug, %error, "stored repository slug is invalid");
                return Ok(true);
            }
        };
        let reservation = match self.take_forge_token(due.forge_id, due.rate_budget).await {
            Ok(reservation) => reservation,
            Err(retry) => {
                self.control.defer_poll(due.repo_id, retry).await?;
                return Ok(false);
            }
        };
        let token = forge_token(due.token_env.as_deref(), api_root.as_str());
        let validators = due.validators();
        match poller
            .poll_repo(
                &self.discovery_client,
                api_root,
                &slug,
                token.as_deref(),
                &validators,
            )
            .await
        {
            Ok(RepoPollOutcome::NotModified {
                validators,
                rate,
                accounting,
            }) => {
                let exhausted_retry = if let Some(cooldown) = rate.exhausted_retry_after() {
                    // The cooldown drains both budgets. Establish it before
                    // any fallible persistence, and never briefly expose a
                    // refund after the Forge reported zero remaining.
                    Some(
                        self.cool_forge_down_for(due.forge_id, due.rate_budget, cooldown)
                            .await,
                    )
                } else {
                    None
                };
                self.record_http_poll(due, cfg, validators, PollHeadObservation::NotModified)
                    .await?;
                if let Some(retry) = exhausted_retry {
                    self.control.defer_poll(due.repo_id, retry).await?;
                    return Ok(false);
                }
                if accounting == ConditionalRequestAccounting::AuthenticatedFree {
                    self.refund_forge_token(due.forge_id, reservation).await?;
                }
                Ok(true)
            }
            Ok(RepoPollOutcome::Head {
                head,
                validators,
                rate,
            }) => {
                let exhausted = if let Some(cooldown) = rate.exhausted_retry_after() {
                    let _ = self
                        .cool_forge_down_for(due.forge_id, due.rate_budget, cooldown)
                        .await;
                    true
                } else {
                    false
                };
                self.record_http_poll(due, cfg, validators, PollHeadObservation::Head(head))
                    .await?;
                if exhausted {
                    return Ok(false);
                }
                Ok(true)
            }
            Err(error) => {
                if let Some(rate_limit) = error.downcast_ref::<ForgeRateLimit>() {
                    let retry = self
                        .cool_forge_down_for(
                            due.forge_id,
                            due.rate_budget,
                            rate_limit.retry_after(),
                        )
                        .await;
                    self.control.defer_poll(due.repo_id, retry).await?;
                    tracing::warn!(slug = %due.slug, "Forge rate-limited the HTTP poll; backing the Forge off");
                    return Ok(false);
                }
                tracing::warn!(slug = %due.slug, error = format!("{error:#}"), "HTTP poll failed; will retry next interval");
                Ok(true)
            }
        }
    }

    async fn record_http_poll(
        &self,
        due: &yg_control::DuePoll,
        cfg: &PollConfig,
        validators: PollValidators,
        observation: PollHeadObservation,
    ) -> anyhow::Result<()> {
        let outcome = self
            .control
            .record_poll_observation(due.repo_id, &validators, &observation)
            .await?;
        match outcome {
            PollRecordOutcome::Unchanged => {}
            PollRecordOutcome::FetchQueued => {
                tracing::info!(slug = %due.slug, "head moved; fetch queued");
            }
            PollRecordOutcome::FetchPending => {
                self.control
                    .defer_poll(due.repo_id, in_flight_fetch_repoll(due, cfg))
                    .await?;
                tracing::info!(slug = %due.slug, "head moved but fetch is already pending; poll retry scheduled");
            }
        }
        Ok(())
    }

    async fn refund_forge_token(
        &self,
        forge_id: i64,
        reservation: ForgeTokenReservation,
    ) -> anyhow::Result<()> {
        if reservation.fallback_reserved {
            self.forge_budgets
                .refund(forge_id, std::num::NonZeroU32::MIN);
        }
        if reservation.shared_reserved {
            self.control
                .refund_forge_budget(forge_id, std::num::NonZeroU32::MIN)
                .await?;
        }
        Ok(())
    }

    /// Spend one of this forge's rate-budget tokens. `Err(retry_after)`
    /// when none is available — the forge is over budget or cooling down
    /// — so the caller can reschedule the repo for `retry_after` out.
    async fn take_forge_token(
        &self,
        forge_id: i64,
        rate_budget: i32,
    ) -> Result<ForgeTokenReservation, Duration> {
        take_shared_or_fallback(&self.control, &self.forge_budgets, forge_id, rate_budget).await
    }

    /// Cool a forge down after it signalled a rate limit, returning when
    /// its next poll should be attempted (past the cooldown).
    async fn cool_forge_down(&self, forge_id: i64, rate_budget: i32) -> Duration {
        self.cool_forge_down_for(forge_id, rate_budget, RATE_LIMIT_COOLDOWN)
            .await
    }

    /// Apply an adapter-provided cooldown duration to the Forge's request
    /// bucket, returning when that bucket may next grant a request.
    async fn cool_forge_down_for(
        &self,
        forge_id: i64,
        rate_budget: i32,
        cooldown: Duration,
    ) -> Duration {
        report_shared_cooldown(
            &self.control,
            &self.forge_budgets,
            forge_id,
            rate_budget,
            cooldown,
        )
        .await
    }
}

enum RepoPollRoute<'a> {
    Http {
        poller: &'a dyn RepoPoller,
        api_root: &'a ForgeUrl,
    },
    Git,
}

fn repo_poll_route<'a>(forge: &'a dyn Forge, api_root: Option<&'a ForgeUrl>) -> RepoPollRoute<'a> {
    match (forge.repo_poller(), api_root) {
        (Some(poller), Some(api_root)) => RepoPollRoute::Http { poller, api_root },
        _ => RepoPollRoute::Git,
    }
}

#[derive(Default)]
struct ForgeRequestBudgets {
    buckets: Mutex<HashMap<i64, TokenBucket>>,
    pending_cooldowns: Mutex<HashMap<i64, Instant>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ForgeTokenReservation {
    shared_reserved: bool,
    fallback_reserved: bool,
}

fn fallback_rate(rate_budget: i32) -> i32 {
    rate_budget.min(FALLBACK_REQUESTS_PER_MINUTE)
}

fn saturating_instant_add(now: Instant, duration: Duration) -> Instant {
    if let Some(deadline) = now.checked_add(duration) {
        return deadline;
    }
    let mut representable = 0_u128;
    let mut unrepresentable = duration.as_nanos();
    while representable + 1 < unrepresentable {
        let candidate = representable + (unrepresentable - representable) / 2;
        let seconds = u64::try_from(candidate / 1_000_000_000)
            .expect("Duration nanoseconds always fit its u64 seconds field");
        let nanoseconds = u32::try_from(candidate % 1_000_000_000)
            .expect("subsecond nanoseconds are below one billion");
        if now
            .checked_add(Duration::new(seconds, nanoseconds))
            .is_some()
        {
            representable = candidate;
        } else {
            unrepresentable = candidate;
        }
    }
    let seconds = u64::try_from(representable / 1_000_000_000)
        .expect("Duration nanoseconds always fit its u64 seconds field");
    let nanoseconds = u32::try_from(representable % 1_000_000_000)
        .expect("subsecond nanoseconds are below one billion");
    now.checked_add(Duration::new(seconds, nanoseconds))
        .expect("the binary search retains only representable deadlines")
}

async fn take_shared_or_fallback(
    control: &ControlPlane,
    fallback_budgets: &ForgeRequestBudgets,
    forge_id: i64,
    rate_budget: i32,
) -> Result<ForgeTokenReservation, Duration> {
    if let Some(retry_after) =
        retry_pending_cooldown_with(fallback_budgets, forge_id, |remaining| {
            control.cool_down_forge(forge_id, remaining)
        })
        .await
    {
        return Err(retry_after);
    }
    if let Some(retry_after) = fallback_budgets.cooldown_retry_after(forge_id) {
        return Err(retry_after);
    }
    resolve_shared_take(
        fallback_budgets,
        forge_id,
        rate_budget,
        control
            .take_forge_budget(forge_id, std::num::NonZeroU32::MIN)
            .await,
    )
}

async fn retry_pending_cooldown_with<Publish, Published>(
    fallback_budgets: &ForgeRequestBudgets,
    forge_id: i64,
    publish: Publish,
) -> Option<Duration>
where
    Publish: FnOnce(Duration) -> Published,
    Published: Future<Output = anyhow::Result<Duration>>,
{
    let (pending_until, retry_after) = fallback_budgets.pending_cooldown_retry_after(forge_id)?;
    match publish(retry_after).await {
        Ok(shared_retry) => {
            fallback_budgets.clear_pending_cooldown_through(forge_id, pending_until);
            Some(shared_retry.max(retry_after))
        }
        Err(error) => {
            tracing::warn!(
                forge_id,
                error = format!("{error:#}"),
                "shared Forge cooldown still unavailable; retaining local cooldown"
            );
            Some(retry_after)
        }
    }
}

fn resolve_shared_take(
    fallback_budgets: &ForgeRequestBudgets,
    forge_id: i64,
    rate_budget: i32,
    shared: anyhow::Result<ForgeBudgetTake>,
) -> Result<ForgeTokenReservation, Duration> {
    match shared {
        Ok(ForgeBudgetTake::Granted) => {
            let fallback_reserved = fallback_budgets
                .take(forge_id, fallback_rate(rate_budget))
                .is_ok();
            Ok(ForgeTokenReservation {
                shared_reserved: true,
                fallback_reserved,
            })
        }
        Ok(ForgeBudgetTake::RetryAfter(retry_after)) => {
            fallback_budgets.cool_down_for(forge_id, fallback_rate(rate_budget), retry_after);
            Err(retry_after)
        }
        Err(error) => {
            tracing::warn!(
                forge_id,
                error = format!("{error:#}"),
                "shared Forge budget unavailable; using stingy fixed-rate local fallback"
            );
            fallback_budgets
                .take(forge_id, fallback_rate(rate_budget))
                .map(|()| ForgeTokenReservation {
                    shared_reserved: false,
                    fallback_reserved: true,
                })
        }
    }
}

async fn report_shared_cooldown(
    control: &ControlPlane,
    fallback_budgets: &ForgeRequestBudgets,
    forge_id: i64,
    rate_budget: i32,
    cooldown: Duration,
) -> Duration {
    let pending = fallback_budgets.pending_cooldown_retry_after(forge_id);
    let pending_retry = pending
        .map(|(_, retry_after)| retry_after)
        .unwrap_or(Duration::ZERO);
    let desired_cooldown = cooldown.max(pending_retry);
    let local_retry =
        fallback_budgets.cool_down_for(forge_id, fallback_rate(rate_budget), desired_cooldown);
    match control.cool_down_forge(forge_id, desired_cooldown).await {
        Ok(shared_retry) => {
            if let Some((pending_until, _)) = pending {
                fallback_budgets.clear_pending_cooldown_through(forge_id, pending_until);
            }
            shared_retry.max(local_retry)
        }
        Err(error) => {
            fallback_budgets.mark_pending_cooldown(forge_id, desired_cooldown);
            tracing::warn!(
                forge_id,
                error = format!("{error:#}"),
                "shared Forge cooldown unavailable; retaining local cooldown"
            );
            local_retry
        }
    }
}

impl ForgeRequestBudgets {
    fn refund(&self, forge_id: i64, token_count: std::num::NonZeroU32) {
        let now = std::time::Instant::now();
        let mut buckets = self.buckets.lock().unwrap();
        if let Some(bucket) = buckets.get_mut(&forge_id) {
            bucket.refund(token_count, now);
        }
    }

    fn pending_cooldown_retry_after(&self, forge_id: i64) -> Option<(Instant, Duration)> {
        let now = Instant::now();
        let mut pending = self
            .pending_cooldowns
            .lock()
            .expect("pending Forge cooldown map poisoned");
        let until = pending.get(&forge_id).copied()?;
        if until <= now {
            pending.remove(&forge_id);
            return None;
        }
        Some((until, until.saturating_duration_since(now)))
    }

    fn mark_pending_cooldown(&self, forge_id: i64, cooldown: Duration) {
        let until = saturating_instant_add(Instant::now(), cooldown);
        let mut pending = self
            .pending_cooldowns
            .lock()
            .expect("pending Forge cooldown map poisoned");
        pending
            .entry(forge_id)
            .and_modify(|existing| *existing = (*existing).max(until))
            .or_insert(until);
    }

    fn clear_pending_cooldown_through(&self, forge_id: i64, published_until: Instant) {
        let mut pending = self
            .pending_cooldowns
            .lock()
            .expect("pending Forge cooldown map poisoned");
        if pending
            .get(&forge_id)
            .is_some_and(|until| *until <= published_until)
        {
            pending.remove(&forge_id);
        }
    }

    fn cooldown_retry_after(&self, forge_id: i64) -> Option<Duration> {
        let now = Instant::now();
        self.buckets
            .lock()
            .expect("forge bucket map poisoned")
            .get(&forge_id)
            .and_then(|bucket| bucket.cooldown_retry_after(now))
    }

    fn take(&self, forge_id: i64, rate_budget: i32) -> Result<(), Duration> {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("forge bucket map poisoned");
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

    fn cool_down_for(&self, forge_id: i64, rate_budget: i32, cooldown: Duration) -> Duration {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("forge bucket map poisoned");
        let bucket = buckets
            .entry(forge_id)
            .or_insert_with(|| TokenBucket::per_minute(rate_budget, now));
        bucket.update_rate(rate_budget, now);
        bucket.cooldown(saturating_instant_add(now, cooldown));
        bucket.retry_after(now)
    }
}

struct DiscoveryBudget<'a> {
    control: &'a ControlPlane,
    fallback_budgets: &'a ForgeRequestBudgets,
    forge_id: i64,
    rate_budget: i32,
}

impl ForgeRequestBudget for DiscoveryBudget<'_> {
    fn take(&self) -> crate::forge::BoxFuture<'_, Result<(), ForgeBudgetExhausted>> {
        Box::pin(async move {
            take_shared_or_fallback(
                self.control,
                self.fallback_budgets,
                self.forge_id,
                self.rate_budget,
            )
            .await
            .map(|_| ())
            .map_err(|retry_after| ForgeBudgetExhausted { retry_after })
        })
    }
}

async fn list_org_repos_with_budget(
    discovery: &dyn OrgDiscovery,
    client: &reqwest::Client,
    api_root: &str,
    org: &str,
    token: Option<&str>,
    budget: &DiscoveryBudget<'_>,
) -> anyhow::Result<Vec<ListedRepo>> {
    let result = discovery
        .list_org_repos_budgeted(client, api_root, org, token, budget)
        .await;
    if let Err(error) = &result
        && let Some(rate_limit) = error.downcast_ref::<ForgeRateLimit>()
    {
        report_shared_cooldown(
            budget.control,
            budget.fallback_budgets,
            budget.forge_id,
            budget.rate_budget,
            rate_limit.retry_after(),
        )
        .await;
    }
    result
}

enum ShutdownClaim<T, M> {
    Empty,
    Ready { job: T, timer: M },
    Released { timer: M },
}

async fn claim_due_fetch_with_optional_shutdown<T, M>(
    shutdown: Option<&Shutdown>,
    claim: impl Future<Output = anyhow::Result<Option<T>>>,
    release: impl AsyncFnOnce(&T) -> anyhow::Result<bool>,
    start_timer: impl FnOnce() -> M,
) -> anyhow::Result<ShutdownClaim<T, M>> {
    let Some(job) = claim.await? else {
        return Ok(ShutdownClaim::Empty);
    };
    let timer = start_timer();
    if shutdown.is_some_and(|shutdown| shutdown.request().is_some()) {
        let released = release(&job).await?;
        tracing::info!(released, "released fresh fetch claim for shutdown");
        return Ok(ShutdownClaim::Released { timer });
    }
    Ok(ShutdownClaim::Ready { job, timer })
}

async fn cancel_discovery_on_shutdown<T>(
    mut shutdown: Shutdown,
    discovery: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<Option<T>> {
    tokio::select! {
        result = discovery => result.map(Some),
        _ = shutdown.requested() => Ok(None),
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::time::Instant;

    use super::*;
    use crate::{ShutdownCause, shutdown_channel};

    #[test]
    fn fallback_rate_is_capped_per_worker() {
        assert_eq!(fallback_rate(300), 2);
        assert_eq!(fallback_rate(4), 2);
        assert_eq!(fallback_rate(3), 2);
        assert_eq!(fallback_rate(1), 1);
    }

    #[test]
    fn local_cooldown_deadlines_saturate_instead_of_panicking() {
        let now = std::time::Instant::now();
        assert_eq!(
            saturating_instant_add(now, Duration::from_secs(30)),
            now + Duration::from_secs(30)
        );

        let saturated = saturating_instant_add(now, Duration::MAX);
        assert!(saturated > now);
        assert!(
            saturated.checked_add(Duration::from_nanos(1)).is_none(),
            "an oversized delay saturates at the platform's latest monotonic deadline"
        );
    }

    #[test]
    fn control_error_uses_only_the_fixed_fallback_burst() {
        let budgets = ForgeRequestBudgets::default();
        for request in 0..2 {
            assert!(
                resolve_shared_take(&budgets, 7, 8, Err(anyhow::anyhow!("control unavailable")),)
                    .is_ok(),
                "fixed fallback should grant request {request} from its opening burst"
            );
        }
        assert!(
            resolve_shared_take(&budgets, 7, 8, Err(anyhow::anyhow!("control unavailable")),)
                .is_err(),
            "the per-worker fallback must expose only a 2-token opening burst"
        );
    }

    #[test]
    fn fallback_grants_never_claim_a_shared_reservation() {
        let budgets = ForgeRequestBudgets::default();
        let reservation =
            resolve_shared_take(&budgets, 7, 8, Err(anyhow::anyhow!("control unavailable")))
                .expect("the opening local fallback token is available");

        assert_eq!(
            reservation,
            ForgeTokenReservation {
                shared_reserved: false,
                fallback_reserved: true,
            }
        );
    }

    #[tokio::test]
    async fn a_failed_cooldown_publication_is_retried_before_another_take() {
        let budgets = ForgeRequestBudgets::default();
        budgets.cool_down_for(7, fallback_rate(60), Duration::from_secs(30));
        budgets.mark_pending_cooldown(7, Duration::from_secs(30));
        let retry_after = retry_pending_cooldown_with(&budgets, 7, |remaining| async move {
            assert!(remaining > Duration::from_secs(29));
            Ok(remaining)
        })
        .await
        .expect("the pending cooldown must intercept the next take");
        assert!(retry_after > Duration::from_secs(29));
        assert!(
            budgets.pending_cooldown_retry_after(7).is_none(),
            "a successful recovery publication clears the pending marker"
        );
        assert!(
            resolve_shared_take(&budgets, 7, 60, Ok(ForgeBudgetTake::Granted)).is_ok(),
            "a local cooldown interleaving after the pre-take guard must not discard a spent shared token"
        );
        assert!(
            budgets.take(7, fallback_rate(60)).is_err(),
            "shadow-spending a shared grant must not clear the local cooldown"
        );
    }

    #[test]
    fn fallback_consumers_spend_the_same_forge_budget() {
        let budgets = ForgeRequestBudgets::default();
        assert!(
            budgets.take(7, 1).is_ok(),
            "the polling seam takes the burst token"
        );

        let exhausted = budgets
            .take(7, 1)
            .expect_err("every fallback consumer must observe the token already spent");
        assert!(exhausted > Duration::ZERO);
    }

    #[test]
    fn a_free_poll_refunds_the_local_fallback_reservation() {
        let budgets = ForgeRequestBudgets::default();
        assert!(budgets.take(7, 1).is_ok());
        assert!(budgets.take(7, 1).is_err());

        budgets.refund(7, std::num::NonZeroU32::MIN);

        assert!(
            budgets.take(7, 1).is_ok(),
            "a Forge-declared free response restores its provisional token"
        );
    }

    #[test]
    fn adapter_cooldown_blocks_the_local_fallback_budget() {
        let budgets = ForgeRequestBudgets::default();
        let cooldown = Duration::from_secs(30);
        let retry_after = budgets.cool_down_for(7, 60, cooldown);
        assert!(retry_after <= cooldown);
        assert!(retry_after > Duration::from_secs(29));

        assert!(budgets.take(7, 60).is_err());
    }

    #[tokio::test]
    async fn fetch_claim_completing_after_shutdown_is_released_before_work_starts() {
        let (trigger, shutdown) = shutdown_channel();
        let released_job = AtomicUsize::new(0);
        let claim = async {
            assert!(trigger.request(
                Instant::now() + Duration::from_secs(30),
                ShutdownCause::Signal,
            ));
            Ok(Some(41_usize))
        };

        let claimed = claim_due_fetch_with_optional_shutdown(
            Some(&shutdown),
            claim,
            async |job| {
                released_job.store(*job, Ordering::SeqCst);
                Ok(true)
            },
            || 73_usize,
        )
        .await
        .expect("post-claim shutdown check");

        assert!(
            matches!(claimed, ShutdownClaim::Released { timer: 73 }),
            "shutdown claims must be released with their timer"
        );
        assert_eq!(released_job.load(Ordering::SeqCst), 41);
    }

    #[tokio::test]
    async fn shutdown_cancels_a_discovery_wait() {
        let (trigger, shutdown) = shutdown_channel();
        assert!(trigger.request(
            Instant::now() + Duration::from_secs(30),
            ShutdownCause::Signal,
        ));

        let result = cancel_discovery_on_shutdown(shutdown, async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .await
        .expect("shutdown-aware discovery wait");

        assert!(result.is_none());
    }
}
