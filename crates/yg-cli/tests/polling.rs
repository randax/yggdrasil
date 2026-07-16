//! Continuous Sync: the poll loop notices a pushed change on a repo's
//! default branch and enqueues the fetch that re-indexes it, with no
//! manual `repo add` (RFC 0001 §3, issue #9). Runs against the dev
//! compose stack like the other e2e targets (see docs/DEVELOPMENT.md).

mod common;

use common::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, atomic::AtomicUsize};
use std::time::Duration;

/// A poll config with a real interval, for tests that drive `poll_once`
/// directly: the fixture repo is already due (its `next_poll_at` defaults
/// to registration time), so the interval here only governs *re*-polling,
/// which these single-poll tests never reach.
fn poll_config() -> yg_sync::PollConfig {
    yg_sync::PollConfig {
        default_interval: Duration::from_secs(300),
        jitter_fraction: 0.2,
    }
}

struct FakeHttpForge {
    outcomes: Mutex<VecDeque<yg_sync::forge::RepoPollOutcome>>,
    validators_seen: Mutex<Vec<yg_control::PollValidators>>,
    calls: AtomicUsize,
}

impl FakeHttpForge {
    fn new(outcomes: impl IntoIterator<Item = yg_sync::forge::RepoPollOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            validators_seen: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl yg_sync::forge::Forge for FakeHttpForge {
    fn kind(&self) -> &'static str {
        "fake-http"
    }

    fn claims_host(&self, _host: &str) -> bool {
        false
    }

    fn default_token_env(&self) -> Option<&'static str> {
        None
    }

    fn default_api_root(&self, _base_url: &str) -> Option<String> {
        None
    }

    fn is_rate_limit(&self, _message: &str) -> bool {
        false
    }

    fn discovery(&self) -> Option<&dyn yg_sync::forge::OrgDiscovery> {
        None
    }

    fn repo_poller(&self) -> Option<&dyn yg_sync::forge::RepoPoller> {
        Some(self)
    }
}

impl yg_sync::forge::RepoPoller for FakeHttpForge {
    fn poll_repo<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        _api_root: &'a yg_control::ForgeUrl,
        _slug: &'a yg_sync::forge::RepoSlug,
        _token: Option<&'a str>,
        validators: &'a yg_control::PollValidators,
    ) -> yg_sync::forge::BoxFuture<'a, anyhow::Result<yg_sync::forge::RepoPollOutcome>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.validators_seen
                .lock()
                .unwrap()
                .push(validators.clone());
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("fake HTTP Forge has no queued poll outcome"))
        })
    }
}

async fn http_poll_worker(
    h: &Harness,
    forge: Arc<FakeHttpForge>,
    rate_budget: i32,
) -> yg_sync::SyncWorker {
    sqlx::query(
        "UPDATE forges
         SET kind = 'fake-http', api_root = 'https://api.example.test', rate_budget = $1",
    )
    .bind(rate_budget)
    .execute(&h.pool().await)
    .await
    .unwrap();
    yg_sync::SyncWorker::with_registry(
        control_plane(&h.db_name).await,
        h.cache.join("fake-http-cache"),
        yg_sync::forge::ForgeRegistry::builtin().register(forge),
    )
}

impl Harness {
    /// Commit `files` onto the fixture's default branch, standing in for a
    /// push to the Forge the poll loop watches.
    fn push_commit(&self, message: &str, files: &[(&str, &str)]) {
        for (path, contents) in files {
            let full = self.repo_dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
        git(&self.repo_dir, &["add", "."]);
        git(&self.repo_dir, &["commit", "-m", message]);
    }

    /// A pool on the harness database, for SQL-level assertions.
    async fn pool(&self) -> sqlx::PgPool {
        sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", self.db_name))
            .await
            .unwrap()
    }

    /// Seconds until this repo is next due for a poll: `next_poll_at -
    /// now()` straight from the row (negative once it is overdue).
    async fn next_poll_in_secs(&self) -> f64 {
        let (secs,): (f64,) =
            sqlx::query_as("SELECT extract(epoch FROM (next_poll_at - now()))::float8 FROM repos")
                .fetch_one(&self.pool().await)
                .await
                .unwrap();
        secs
    }

    /// The repo's recorded sync position — its `last_synced_commit`.
    async fn synced_commit(&self) -> Option<String> {
        let (commit,): (Option<String>,) = sqlx::query_as("SELECT last_synced_commit FROM repos")
            .fetch_one(&self.pool().await)
            .await
            .unwrap();
        commit
    }

    /// How many Shard rows the control plane records.
    async fn shard_row_count(&self) -> i64 {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM shards")
            .fetch_one(&self.pool().await)
            .await
            .unwrap();
        n
    }

    /// Total git objects (loose + packed) in the worker's one mirror —
    /// proof that a poll moved no objects (a conditional request does not
    /// transfer any).
    fn mirror_object_count(&self) -> u64 {
        let out = git(&only_mirror(&self.cache), &["count-objects", "-v"]);
        let field = |key: &str| {
            out.lines()
                .find_map(|line| line.strip_prefix(key)?.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        field("count:") + field("in-pack:")
    }

    /// Whether the `search` Verb finds a Symbol by this exact name — the
    /// observable "is it in the Knowledge Graph yet?" check.
    async fn symbol_is_queryable(&self, name: &str) -> bool {
        let body = self
            .verb_ok("search", serde_json::json!({"query": name}))
            .await;
        body["hits"]
            .as_array()
            .is_some_and(|hits| hits.iter().any(|h| h["name"].as_str() == Some(name)))
    }
}

#[tokio::test]
async fn a_pushed_commit_becomes_queryable_after_a_poll() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    assert!(
        h.symbol_is_queryable("Hello").await,
        "the first synced commit is queryable"
    );
    assert!(
        !h.symbol_is_queryable("Greet").await,
        "the new symbol does not exist yet"
    );

    // A push lands on the default branch — no `repo add`, no manual fetch.
    h.push_commit(
        "add Greet",
        &[(
            "extra.go",
            "package main\n\nfunc Greet() string {\n\treturn \"hi\"\n}\n",
        )],
    );

    // The poll loop notices the moved head and enqueues a fetch; draining
    // the queues re-indexes the repo to a fresh Shard revision.
    assert!(
        h.sync.poll_once(&poll_config()).await.unwrap(),
        "a due repo whose head moved must be polled and enqueue work"
    );
    h.sync_and_index().await;

    assert!(
        h.symbol_is_queryable("Greet").await,
        "the pushed symbol is queryable within one poll interval"
    );
}

#[tokio::test]
async fn a_poll_that_races_an_existing_fetch_retries_soon_if_that_fetch_was_stale() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let first_head = h
        .synced_commit()
        .await
        .expect("the fixture has been synced");

    // A manual re-add queues a fetch, and a worker claims it before the
    // push below. This leased fetch stands in for one that already read
    // the old remote head and will complete stale.
    h.add_repo().await;
    let control = control_plane(&h.db_name).await;
    let stale_fetch = control
        .claim_due_fetch(Duration::from_secs(60))
        .await
        .unwrap()
        .expect("re-add queued a fetch");

    h.push_commit(
        "add Greet",
        &[(
            "extra.go",
            "package main\n\nfunc Greet() string {\n\treturn \"hi\"\n}\n",
        )],
    );

    assert!(
        h.sync.poll_once(&poll_config()).await.unwrap(),
        "the poll observes the moved head"
    );
    let retry = h.next_poll_in_secs().await;
    assert!(
        (-1.0..=35.0).contains(&retry),
        "a moved head hidden behind an in-flight fetch must retry soon, not after the full interval; got {retry}s"
    );

    assert!(
        control
            .complete_fetch(&stale_fetch, &first_head)
            .await
            .unwrap(),
        "the already-leased fetch completes with the old head"
    );
    assert!(
        !h.symbol_is_queryable("Greet").await,
        "the stale fetch must not have indexed the pushed symbol"
    );

    // Time-travel the near-term retry to avoid sleeping in the test. The
    // second poll sees the head still moved, queues a fresh fetch, and the
    // normal fetch+index pipeline catches up.
    sqlx::query("UPDATE repos SET next_poll_at = now()")
        .execute(&h.pool().await)
        .await
        .unwrap();
    assert!(
        h.sync.poll_once(&poll_config()).await.unwrap(),
        "the retry poll queues the missing fetch"
    );
    assert!(h.sync.run_once().await.unwrap(), "fresh fetch runs");
    assert!(h.indexer.run_once().await.unwrap(), "fresh index runs");
    assert!(
        h.symbol_is_queryable("Greet").await,
        "the pushed symbol becomes queryable after the retry"
    );
}

#[tokio::test]
async fn a_304_retries_a_moved_head_hidden_behind_a_stale_fetch_lease() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let first_head = h
        .synced_commit()
        .await
        .expect("the fixture has been synced");
    let repo_id: i64 = sqlx::query_scalar("SELECT id FROM repos")
        .fetch_one(&h.pool().await)
        .await
        .unwrap();

    h.add_repo().await;
    let control = control_plane(&h.db_name).await;
    let stale_fetch = control
        .claim_due_fetch(Duration::from_secs(60))
        .await
        .unwrap()
        .expect("the old-head fetch is leased before polling sees the move");

    let moved_head =
        yg_sync::forge::CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
    let moved_validators = yg_control::PollValidators {
        etag: Some(yg_control::PollEtag::new(b"\"head-b\"")),
        last_modified: None,
    };
    let not_modified = || yg_sync::forge::RepoPollOutcome::NotModified {
        validators: moved_validators.clone(),
        rate: yg_sync::forge::ForgeRateObservation::default(),
        accounting: yg_sync::forge::ConditionalRequestAccounting::AuthenticatedFree,
    };
    let forge = Arc::new(FakeHttpForge::new([
        yg_sync::forge::RepoPollOutcome::Head {
            head: moved_head.clone(),
            validators: moved_validators.clone(),
            rate: yg_sync::forge::ForgeRateObservation::default(),
        },
        not_modified(),
        not_modified(),
    ]));
    let worker = http_poll_worker(&h, forge.clone(), 10).await;

    assert!(worker.poll_once(&poll_config()).await.unwrap());
    assert!(
        control
            .complete_fetch(&stale_fetch, &first_head)
            .await
            .unwrap(),
        "the stale fetch completes without advancing the sync position"
    );

    sqlx::query("UPDATE repos SET next_poll_at = now() WHERE id = $1")
        .bind(repo_id)
        .execute(&h.pool().await)
        .await
        .unwrap();
    assert!(
        worker.poll_once(&poll_config()).await.unwrap(),
        "the next 304 must enqueue the persisted moved head after the lease clears"
    );
    let moved_fetch = control
        .claim_due_fetch(Duration::from_secs(60))
        .await
        .unwrap()
        .expect("exactly one fresh fetch is queued for the moved head");
    assert!(
        control
            .complete_fetch(&moved_fetch, moved_head.as_str())
            .await
            .unwrap()
    );

    sqlx::query("UPDATE repos SET next_poll_at = now() WHERE id = $1")
        .bind(repo_id)
        .execute(&h.pool().await)
        .await
        .unwrap();
    assert!(worker.poll_once(&poll_config()).await.unwrap());
    assert!(
        control
            .claim_due_fetch(Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "once the moved head is synced, another 304 must not enqueue it again"
    );
    assert_eq!(forge.calls.load(Ordering::Relaxed), 3);
    assert_eq!(
        forge.validators_seen.lock().unwrap().as_slice(),
        &[
            yg_control::PollValidators::default(),
            moved_validators.clone(),
            moved_validators,
        ],
        "the moved validators advance while the observed head remains pending"
    );
}

#[tokio::test]
async fn an_unchanged_head_costs_only_a_conditional_request() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let synced_before = h.synced_commit().await;
    let objects_before = h.mirror_object_count();
    let shards_before = h.shard_row_count().await;

    // Nothing pushed between syncing and polling: the head has not moved.
    assert!(
        h.sync.poll_once(&poll_config()).await.unwrap(),
        "the repo was due to be polled"
    );

    // The conditional request found no change, so no fetch was queued —
    // the next fetch run finds nothing to do and the sync position holds.
    assert!(
        !h.sync.run_once().await.unwrap(),
        "an unchanged head must queue no fetch (only the conditional request)"
    );
    assert_eq!(
        h.synced_commit().await,
        synced_before,
        "an unchanged head must not move the sync position"
    );
    // "Only a conditional request": the poll transferred no git objects
    // into the mirror, and published no new Shard revision.
    assert_eq!(
        h.mirror_object_count(),
        objects_before,
        "a conditional request must transfer no git objects"
    );
    assert_eq!(
        h.shard_row_count().await,
        shards_before,
        "an unchanged head must publish no new Shard revision"
    );
}

#[tokio::test]
async fn a_github_304_reuses_validators_queues_no_fetch_and_refunds_its_budget() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo_id: i64 = sqlx::query_scalar("SELECT id FROM repos")
        .fetch_one(&h.pool().await)
        .await
        .unwrap();
    let validators = yg_control::PollValidators {
        etag: Some(yg_control::PollEtag::new(b"\"head-v1\"".to_vec())),
        last_modified: None,
    };
    control_plane(&h.db_name)
        .await
        .record_poll_observation(
            repo_id,
            &validators,
            &yg_control::PollHeadObservation::NotModified,
        )
        .await
        .unwrap();
    let unchanged = || yg_sync::forge::RepoPollOutcome::NotModified {
        validators: validators.clone(),
        rate: yg_sync::forge::ForgeRateObservation::default(),
        accounting: yg_sync::forge::ConditionalRequestAccounting::AuthenticatedFree,
    };
    let forge = Arc::new(FakeHttpForge::new([unchanged(), unchanged()]));
    let worker = http_poll_worker(&h, forge.clone(), 1).await;

    assert!(worker.poll_once(&poll_config()).await.unwrap());
    sqlx::query("UPDATE repos SET next_poll_at = now() WHERE id = $1")
        .bind(repo_id)
        .execute(&h.pool().await)
        .await
        .unwrap();
    assert!(
        worker.poll_once(&poll_config()).await.unwrap(),
        "the second immediate 304 proves the one-token shared budget was refunded"
    );

    let pending_fetches: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'fetch' AND state <> 'done'")
            .fetch_one(&h.pool().await)
            .await
            .unwrap();
    assert_eq!(pending_fetches, 0, "304 must not enqueue a fetch");
    assert_eq!(forge.calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        forge.validators_seen.lock().unwrap().as_slice(),
        &[validators.clone(), validators],
        "every conditional poll receives the persisted ETag"
    );
}

#[tokio::test]
async fn an_unauthenticated_304_consumes_the_configured_budget() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo_id: i64 = sqlx::query_scalar("SELECT id FROM repos")
        .fetch_one(&h.pool().await)
        .await
        .unwrap();
    let charged = || yg_sync::forge::RepoPollOutcome::NotModified {
        validators: yg_control::PollValidators::default(),
        rate: yg_sync::forge::ForgeRateObservation::default(),
        accounting: yg_sync::forge::ConditionalRequestAccounting::Charged,
    };
    let forge = Arc::new(FakeHttpForge::new([charged(), charged()]));
    let worker = http_poll_worker(&h, forge.clone(), 1).await;

    assert!(worker.poll_once(&poll_config()).await.unwrap());
    sqlx::query("UPDATE repos SET next_poll_at = now() WHERE id = $1")
        .bind(repo_id)
        .execute(&h.pool().await)
        .await
        .unwrap();
    assert!(
        !worker.poll_once(&poll_config()).await.unwrap(),
        "a charged 304 must not restore the one-token budget"
    );
    assert_eq!(
        forge.calls.load(Ordering::Relaxed),
        1,
        "the depleted budget blocks the second conditional request"
    );
}

#[tokio::test]
async fn a_free_304_still_honors_an_exhausted_primary_limit_reset() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let forge = Arc::new(FakeHttpForge::new([
        yg_sync::forge::RepoPollOutcome::NotModified {
            validators: yg_control::PollValidators::default(),
            rate: yg_sync::forge::ForgeRateObservation {
                remaining: Some(yg_sync::forge::RateLimitRemaining::new(0)),
                reset: Some(yg_sync::forge::RateLimitReset::after(Duration::from_secs(
                    30,
                ))),
            },
            accounting: yg_sync::forge::ConditionalRequestAccounting::AuthenticatedFree,
        },
    ]));
    let worker = http_poll_worker(&h, forge, 60).await;

    assert!(
        !worker.poll_once(&poll_config()).await.unwrap(),
        "the reset cooldown stops the poll loop after processing the free 304"
    );
    let retry = h.next_poll_in_secs().await;
    assert!(
        (29.0..=35.0).contains(&retry),
        "the repo must retry after the server reset plus refill, got {retry}s"
    );
}

#[tokio::test]
async fn a_github_200_persists_validators_and_enqueues_exactly_one_fetch() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo_id: i64 = sqlx::query_scalar("SELECT id FROM repos")
        .fetch_one(&h.pool().await)
        .await
        .unwrap();
    let validators = yg_control::PollValidators {
        etag: Some(yg_control::PollEtag::new(b"\"head-v2\"".to_vec())),
        last_modified: Some(yg_control::PollLastModified::new(
            b"Thu, 22 Oct 2015 07:28:00 GMT".to_vec(),
        )),
    };
    let moved = || yg_sync::forge::RepoPollOutcome::Head {
        head: yg_sync::forge::CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
        validators: validators.clone(),
        rate: yg_sync::forge::ForgeRateObservation::default(),
    };
    let forge = Arc::new(FakeHttpForge::new([moved(), moved()]));
    let worker = http_poll_worker(&h, forge, 10).await;

    assert!(worker.poll_once(&poll_config()).await.unwrap());
    sqlx::query("UPDATE repos SET next_poll_at = now() WHERE id = $1")
        .bind(repo_id)
        .execute(&h.pool().await)
        .await
        .unwrap();
    assert!(worker.poll_once(&poll_config()).await.unwrap());

    let pending_fetches: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'fetch' AND state <> 'done'")
            .fetch_one(&h.pool().await)
            .await
            .unwrap();
    assert_eq!(
        pending_fetches, 1,
        "repeated moved-head observations coalesce to one in-flight fetch"
    );
    sqlx::query("UPDATE repos SET next_poll_at = now() WHERE id = $1")
        .bind(repo_id)
        .execute(&h.pool().await)
        .await
        .unwrap();
    let claimed = control_plane(&h.db_name)
        .await
        .claim_due_poll(Duration::from_secs(300), 0.0)
        .await
        .unwrap()
        .expect("the repo remains pollable while its fetch is queued");
    assert_eq!(claimed.validators(), validators);
}

#[tokio::test]
async fn http_poll_validators_survive_a_control_plane_restart() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let first = control_plane(&h.db_name).await;
    let repo_id: i64 = sqlx::query_scalar("SELECT id FROM repos")
        .fetch_one(&h.pool().await)
        .await
        .unwrap();
    let expected = yg_control::PollValidators {
        etag: Some(yg_control::PollEtag::new(b"W/\"head-v1\"".to_vec())),
        last_modified: Some(yg_control::PollLastModified::new(
            b"Wed, 21 Oct 2015 07:28:00 GMT".to_vec(),
        )),
    };
    assert_eq!(
        first
            .record_poll_observation(
                repo_id,
                &expected,
                &yg_control::PollHeadObservation::NotModified,
            )
            .await
            .unwrap(),
        yg_control::PollRecordOutcome::Unchanged,
        "an unchanged observation must not enqueue a fetch"
    );
    drop(first);

    sqlx::query("UPDATE repos SET next_poll_at = now() WHERE id = $1")
        .bind(repo_id)
        .execute(&h.pool().await)
        .await
        .unwrap();
    let restarted = control_plane(&h.db_name).await;
    let claimed = restarted
        .claim_due_poll(Duration::from_secs(300), 0.0)
        .await
        .unwrap()
        .expect("the restarted control plane reads the due repo");
    assert_eq!(claimed.validators(), expected);
}

#[tokio::test]
async fn polling_a_repo_advances_its_schedule() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    assert!(
        h.sync.poll_once(&poll_config()).await.unwrap(),
        "the repo was due"
    );
    assert!(
        !h.sync.poll_once(&poll_config()).await.unwrap(),
        "after a poll the repo is scheduled into the future, not due again immediately"
    );
}

#[tokio::test]
async fn the_next_poll_lands_within_the_jittered_window() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    // A 100s interval with 50% jitter schedules the next poll in
    // [100, 150]s; a few ms elapse before the row is read.
    let cfg = yg_sync::PollConfig {
        default_interval: Duration::from_secs(100),
        jitter_fraction: 0.5,
    };
    assert!(h.sync.poll_once(&cfg).await.unwrap(), "the repo was due");

    let gap = h.next_poll_in_secs().await;
    // Lower bound allows a few seconds of elapsed time between the UPDATE's
    // now() and this read (a fresh pool connection on a loaded box).
    assert!(
        (95.0..=150.5).contains(&gap),
        "the next poll must land within [interval, interval·(1+jitter)], got {gap}s"
    );
}

#[tokio::test]
async fn a_per_repo_interval_overrides_the_default() {
    let h = Harness::boot().await;
    // Register with a 1-second poll interval.
    let resp = post_repo(
        &h.base,
        serde_json::json!({"url": h.fixture_url, "poll_interval": 1}),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "registering with a poll interval must succeed, got {}",
        resp.status()
    );
    h.sync_and_index().await;

    // Poll with an hour-long default and zero jitter. The schedule the
    // claim sets reveals which interval governed: ~1s (the per-repo
    // override) versus ~3600s (the default). Asserting the column instead
    // of sleeping out the interval keeps the test fast and non-flaky.
    let long_default = yg_sync::PollConfig {
        default_interval: Duration::from_secs(3600),
        jitter_fraction: 0.0,
    };
    assert!(
        h.sync.poll_once(&long_default).await.unwrap(),
        "the repo was due"
    );

    let gap = h.next_poll_in_secs().await;
    assert!(
        gap <= 5.0,
        "the per-repo 1s interval must govern the schedule, not the 3600s default; got {gap}s"
    );
}

#[tokio::test]
async fn polls_stay_within_the_forge_rate_budget() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    // Tighten the forge to a single conditional request per minute.
    sqlx::query("UPDATE forges SET rate_budget = 1")
        .execute(&h.pool().await)
        .await
        .unwrap();

    // The first poll spends the forge's one token.
    assert!(
        h.sync.poll_once(&poll_config()).await.unwrap(),
        "the first poll claims the forge's one budget token"
    );

    // Force the repo due again at once and move its head.
    sqlx::query("UPDATE repos SET next_poll_at = now()")
        .execute(&h.pool().await)
        .await
        .unwrap();
    h.push_commit(
        "add Greet",
        &[("extra.go", "package main\n\nfunc Greet() {}\n")],
    );

    // Over budget now: the repo is claimed but its head is not checked,
    // so the moved head goes unnoticed this cycle and no fetch is queued.
    // poll_once returns false (a pure defer is not useful work) so the
    // driving loop backs off rather than hot-spinning.
    assert!(
        !h.sync.poll_once(&poll_config()).await.unwrap(),
        "an over-budget poll does no useful work and must report so (back off)"
    );
    assert!(
        !h.sync.run_once().await.unwrap(),
        "over budget: the moved head must not be fetched this cycle"
    );

    // It is rescheduled by the bucket's refill wait — ~60s for a drained
    // 1/min budget. The lower bound is the fingerprint that the budget
    // (not some near-zero reschedule) gated the head check; the upper
    // bound rules out the 300s default interval.
    let gap = h.next_poll_in_secs().await;
    assert!(
        (40.0..=70.0).contains(&gap),
        "an over-budget repo retries on the rate-budget refill window (~60s), got {gap}s"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_queries_never_observe_a_partial_swap() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    // A reader hammers the search Verb while the current-Shard pointer
    // swaps under it. `Hello` is in main.go in every revision, so a torn
    // read — a half-swapped pointer, a Shard deleted mid-read — would
    // either error or drop it.
    let base = h.base.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = stop.clone();
    let reader = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut reads = 0u32;
        while !reader_stop.load(Ordering::Relaxed) {
            let resp = client
                .post(format!("{base}/v1/verbs/search"))
                .bearer_auth(TEST_TOKEN)
                .json(&serde_json::json!({"query": "Hello"}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                200,
                "a query racing a pointer swap must still succeed"
            );
            let body: serde_json::Value = resp.json().await.unwrap();
            assert!(
                body["hits"]
                    .as_array()
                    .is_some_and(|hits| hits.iter().any(|hit| hit["name"] == "Hello")),
                "whichever revision the query resolved must be a whole Shard \
                 that still defines Hello, got: {body}"
            );
            reads += 1;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        reads
    });

    // Re-index several times; each completion atomically swaps the pointer.
    // GC runs (zero grace) right after each swap, so the reader is racing
    // both a pointer swap and the deletion of the just-superseded Shard —
    // the deletion must never touch the Shard the reader resolves.
    for n in 0..5 {
        h.push_commit(
            &format!("rev {n}"),
            &[(
                &format!("f{n}.go"),
                &format!("package main\n\nfunc F{n}() {{}}\n"),
            )],
        );
        h.add_repo().await;
        h.sync_and_index().await;
        h.indexer.gc_once(Duration::ZERO).await.unwrap();
    }

    stop.store(true, Ordering::Relaxed);
    let reads = reader.await.unwrap();
    assert!(
        reads >= 5,
        "the reader must have queried across the swaps (got {reads})"
    );
}

/// multi_thread: the spawned `yg serve` process and the in-test HTTP
/// client both drive this runtime.
#[tokio::test(flavor = "multi_thread")]
async fn the_poll_loop_re_indexes_a_push_with_no_manual_intervention() {
    let (fixture, repo_dir, fixture_url) = go_fixture_repo();
    let db_name = create_test_db().await;
    // A 1-second poll interval so the demo doesn't wait out the default.
    let (_server, url) = spawn_yg_serve(&db_name, |cmd| {
        cmd.env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN)
            .env("YG_GIT_CACHE", fixture.path().join("git-cache"))
            .env("YG_POLL_INTERVAL", "1");
    });

    post_repo(&url, serde_json::json!({"url": fixture_url})).await;
    // The running server syncs and indexes the repo on its own.
    await_symbol(&url, "Hello", Duration::from_secs(30)).await;

    // A push lands on the default branch — no `repo add`, no manual fetch.
    std::fs::write(
        repo_dir.join("extra.go"),
        "package main\n\nfunc Greet() string {\n\treturn \"hi\"\n}\n",
    )
    .unwrap();
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "add Greet"]);

    // The poll loop notices the moved head and re-indexes — the issue's
    // demo: the pushed symbol becomes queryable within one poll interval.
    await_symbol(&url, "Greet", Duration::from_secs(30)).await;
}

/// multi_thread: the spawned `yg` binary blocks a thread while the
/// in-process server it calls runs on this runtime.
#[tokio::test(flavor = "multi_thread")]
async fn the_cli_poll_interval_flag_sets_the_per_repo_schedule() {
    let h = Harness::boot().await;
    // Register through the real CLI flag — exercising clap → HTTP → control
    // plane, not just the API body.
    h.yg_ok(&[
        "admin",
        "repo",
        "add",
        &h.fixture_url,
        "--poll-interval",
        "1",
    ])
    .await;
    h.sync_and_index().await;

    // Poll with an hour-long default and no jitter; the CLI-set 1s interval
    // is what schedules the next poll (~1s, not ~3600s).
    let long_default = yg_sync::PollConfig {
        default_interval: Duration::from_secs(3600),
        jitter_fraction: 0.0,
    };
    assert!(
        h.sync.poll_once(&long_default).await.unwrap(),
        "the repo was due"
    );
    let gap = h.next_poll_in_secs().await;
    assert!(
        gap <= 5.0,
        "the --poll-interval 1 flag must set the per-repo schedule, not the default; got {gap}s"
    );
}

/// A throwaway HTTP server that answers every request with `429 Too Many
/// Requests` — git's `ls-remote` against it fails the way a forge does
/// when rate-limiting us. Returns a clone URL pointing at it.
fn spawn_429_forge() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            // Drain the request line/headers so the client's send completes.
            let _ = stream.read(&mut [0u8; 1024]);
            let _ = stream.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
        }
    });
    format!("http://{addr}/acme/throttled")
}

#[tokio::test]
async fn a_forge_that_rate_limits_a_poll_is_cooled_down() {
    let h = Harness::boot().await;
    // Register a repo on a forge that 429s every request.
    let resp = post_repo(&h.base, serde_json::json!({"url": spawn_429_forge()})).await;
    assert!(
        resp.status().is_success(),
        "registering the throttling forge must succeed, got {}",
        resp.status()
    );
    // Clear the fetch registration queued, and make the repo poll-eligible
    // without a successful fetch (the forge 429s, so it could never sync) —
    // so the only thing that could queue a fetch now is the poll itself.
    sqlx::query("DELETE FROM jobs")
        .execute(&h.pool().await)
        .await
        .unwrap();
    sqlx::query("UPDATE repos SET last_synced_commit = 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeef'")
        .execute(&h.pool().await)
        .await
        .unwrap();

    // The poll's ls-remote hits the 429: the forge is recognized as
    // rate-limiting and cooled down, the repo rescheduled past the cooldown
    // (~5 min) rather than the normal interval — and the poll backs off.
    assert!(
        !h.sync.poll_once(&poll_config()).await.unwrap(),
        "a rate-limited poll does no useful work and backs off"
    );
    assert!(
        !h.sync.run_once().await.unwrap(),
        "a rate-limited poll must queue no fetch"
    );
    let gap = h.next_poll_in_secs().await;
    assert!(
        (280.0..=305.0).contains(&gap),
        "a rate-limited forge is cooled down for ~5 minutes, got {gap}s"
    );
}
