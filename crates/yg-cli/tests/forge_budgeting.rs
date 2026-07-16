mod common;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use common::{DEV_POSTGRES, control_plane, create_test_db, spawn_yg_worker};
use yg_control::ForgeBudgetTake;
use yg_sync::forge::{BoxFuture, Forge, ForgeRateLimit, ForgeRegistry, ListedRepo, OrgDiscovery};

struct RateLimitedDiscoveryForge;

impl Forge for RateLimitedDiscoveryForge {
    fn kind(&self) -> &'static str {
        "rate-limited-discovery"
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

    fn discovery(&self) -> Option<&dyn OrgDiscovery> {
        Some(self)
    }
}

impl OrgDiscovery for RateLimitedDiscoveryForge {
    fn list_org_repos<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        _api_root: &'a str,
        _org: &'a str,
        _token: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>> {
        Box::pin(async {
            Err(ForgeRateLimit::new(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                Duration::from_secs(30),
            )
            .into())
        })
    }
}

async fn low_budget_forge(db_name: &str) -> (sqlx::PgPool, i64) {
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    let forge_id = sqlx::query_scalar(
        "INSERT INTO forges (kind, base_url, rate_budget)
         VALUES ('git', 'https://shared-budget.example.test', 1)
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    (pool, forge_id)
}

#[tokio::test]
async fn concurrent_workers_draw_from_one_forge_budget() {
    let db_name = create_test_db().await;
    let first = control_plane(&db_name).await;
    let second = control_plane(&db_name).await;
    let (_pool, forge_id) = low_budget_forge(&db_name).await;

    let (left, right) = tokio::join!(
        first.take_forge_budget(forge_id, NonZeroU32::MIN),
        second.take_forge_budget(forge_id, NonZeroU32::MIN),
    );
    let decisions = [left.unwrap(), right.unwrap()];
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| matches!(decision, ForgeBudgetTake::Granted))
            .count(),
        1,
        "two workers must not each receive the single shared burst token"
    );
    let retry_after = decisions
        .iter()
        .find_map(|decision| match decision {
            ForgeBudgetTake::Granted => None,
            ForgeBudgetTake::RetryAfter(delay) => Some(*delay),
        })
        .expect("one worker must be deferred");
    assert!(
        (Duration::from_secs(40)..=Duration::from_secs(60)).contains(&retry_after),
        "the denied worker should wait for the shared 1/min refill, got {retry_after:?}"
    );
}

#[tokio::test]
async fn a_refunded_shared_reservation_is_immediately_available() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let (_pool, forge_id) = low_budget_forge(&db_name).await;

    assert_eq!(
        control
            .take_forge_budget(forge_id, NonZeroU32::MIN)
            .await
            .unwrap(),
        ForgeBudgetTake::Granted
    );
    assert!(
        matches!(
            control
                .take_forge_budget(forge_id, NonZeroU32::MIN)
                .await
                .unwrap(),
            ForgeBudgetTake::RetryAfter(_)
        ),
        "the one-token bucket must be empty before the refund"
    );

    control
        .refund_forge_budget(forge_id, NonZeroU32::MIN)
        .await
        .unwrap();
    assert_eq!(
        control
            .take_forge_budget(forge_id, NonZeroU32::MIN)
            .await
            .unwrap(),
        ForgeBudgetTake::Granted,
        "a free response must restore its reserved shared token"
    );
}

#[tokio::test]
async fn a_reservation_larger_than_capacity_is_rejected_without_spending() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let (_pool, forge_id) = low_budget_forge(&db_name).await;

    let error = control
        .take_forge_budget(forge_id, NonZeroU32::new(2).unwrap())
        .await
        .expect_err("a request larger than the bucket can never become retryable");
    assert!(
        error
            .downcast_ref::<yg_control::ForgeBudgetRequestTooLarge>()
            .is_some(),
        "oversized reservations must use the typed control-plane error"
    );
    assert_eq!(
        control
            .take_forge_budget(forge_id, NonZeroU32::MIN)
            .await
            .unwrap(),
        ForgeBudgetTake::Granted,
        "rejecting an impossible reservation must leave the opening token untouched"
    );
}

#[tokio::test]
async fn a_rate_limit_reported_by_one_worker_cools_the_fleet() {
    let db_name = create_test_db().await;
    let first = control_plane(&db_name).await;
    let second = control_plane(&db_name).await;
    let (pool, forge_id) = low_budget_forge(&db_name).await;

    let cooldown_retry = first
        .cool_down_forge(forge_id, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(
        (Duration::from_secs(85)..=Duration::from_secs(90)).contains(&cooldown_retry),
        "a drained fleet bucket should include cooldown plus fresh refill, got {cooldown_retry:?}"
    );
    let decision = second
        .take_forge_budget(forge_id, NonZeroU32::MIN)
        .await
        .unwrap();
    let ForgeBudgetTake::RetryAfter(retry_after) = decision else {
        panic!("a second worker must observe the fleet-wide cooldown");
    };
    assert!(
        (Duration::from_secs(85)..=Duration::from_secs(90)).contains(&retry_after),
        "a drained fleet bucket should include cooldown plus fresh refill, got {retry_after:?}"
    );

    let (tokens, refill_origin_gap): (f64, f64) = sqlx::query_as(
        "SELECT tokens,
                abs(extract(epoch FROM updated_at - cooldown_until))::float8
         FROM forge_rate_budgets
         WHERE forge_id = $1",
    )
    .bind(forge_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tokens, 0.0, "cooldown must drain the shared bucket");
    assert!(
        refill_origin_gap < 0.01,
        "refill must start at the cooldown deadline, timestamp gap was {refill_origin_gap}s"
    );

    sqlx::query(
        "UPDATE forge_rate_budgets
         SET cooldown_until = clock_timestamp() - interval '10 milliseconds',
             updated_at = clock_timestamp() - interval '10 milliseconds'
         WHERE forge_id = $1",
    )
    .bind(forge_id)
    .execute(&pool)
    .await
    .unwrap();
    let decision = second
        .take_forge_budget(forge_id, NonZeroU32::MIN)
        .await
        .unwrap();
    let ForgeBudgetTake::RetryAfter(retry_after) = decision else {
        panic!("cooldown expiry must resume from an empty shared bucket");
    };
    assert!(
        (Duration::from_secs(50)..=Duration::from_secs(60)).contains(&retry_after),
        "the first post-cooldown token should require a fresh refill, got {retry_after:?}"
    );
}

#[tokio::test]
async fn a_typed_discovery_rate_limit_is_published_to_other_workers() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let connected = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "rate-limited-discovery",
            base_url: "https://discovery-limit.example.test",
            org_slug: "acme",
            token_env: None,
            api_root: Some("https://discovery-limit.example.test/api"),
        })
        .await
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let worker = yg_sync::SyncWorker::with_registry(
        control_plane(&db_name).await,
        cache.path(),
        ForgeRegistry::builtin().register(Arc::new(RateLimitedDiscoveryForge)),
    );

    assert!(
        worker
            .discover_once(&yg_sync::DiscoveryConfig {
                interval: Duration::from_secs(3600),
            })
            .await
            .unwrap(),
        "the discovery org must be claimed"
    );
    let decision = control_plane(&db_name)
        .await
        .take_forge_budget(connected.forge_id, NonZeroU32::MIN)
        .await
        .unwrap();
    let ForgeBudgetTake::RetryAfter(retry_after) = decision else {
        panic!("another worker must observe the typed adapter cooldown");
    };
    assert!(
        (Duration::from_secs(25)..=Duration::from_secs(31)).contains(&retry_after),
        "the typed discovery cooldown should include its 300/min fresh refill, got {retry_after:?}"
    );
}

fn spawn_rate_limited_forge() -> (String, Arc<AtomicUsize>) {
    use std::io::{Read, Write};

    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            observed.fetch_add(1, Ordering::Relaxed);
            let mut stream = stream;
            let _ = stream.read(&mut [0_u8; 1024]);
            let _ = stream.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
        }
    });
    (format!("http://{address}"), requests)
}

#[tokio::test]
async fn a_worker_rate_limit_response_blocks_another_worker_before_its_forge_call() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let (base_url, requests) = spawn_rate_limited_forge();
    for slug in ["acme/one", "acme/two"] {
        control
            .add_repo(yg_control::AddRepo {
                forge_kind: "git",
                base_url: &base_url,
                token_env: None,
                api_root: None,
                slug,
                fetch_depth: None,
                poll_interval_seconds: None,
            })
            .await
            .unwrap();
    }
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    sqlx::query("DELETE FROM jobs")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE repos
         SET last_synced_commit = 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeef'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let first_cache = tempfile::tempdir().unwrap();
    let second_cache = tempfile::tempdir().unwrap();
    let first = yg_sync::SyncWorker::new(control_plane(&db_name).await, first_cache.path());
    let second = yg_sync::SyncWorker::new(control_plane(&db_name).await, second_cache.path());
    let poll = yg_sync::PollConfig {
        default_interval: Duration::from_secs(3600),
        jitter_fraction: 0.0,
    };

    assert!(!first.poll_once(&poll).await.unwrap());
    let requests_after_limit = requests.load(Ordering::Relaxed);
    assert!(requests_after_limit > 0, "worker A must reach the Forge");
    assert!(!second.poll_once(&poll).await.unwrap());
    assert_eq!(
        requests.load(Ordering::Relaxed),
        requests_after_limit,
        "worker B must observe worker A's fleet cooldown before a Forge call"
    );
}

fn spawn_blocking_forge() -> (String, Arc<AtomicUsize>, Arc<AtomicBool>) {
    use std::io::{Read, Write};

    let requests = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let observed = requests.clone();
    let server_release = release.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            observed.fetch_add(1, Ordering::Relaxed);
            let request_release = server_release.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.read(&mut [0_u8; 1024]);
                while !request_release.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
            });
        }
    });
    (format!("http://{address}"), requests, release)
}

/// Process-level coverage for the topology behind the control-plane seam.
/// This target is compiled in the normal suite and run with the Docker e2e
/// stack, alongside the other spawned-role tests.
#[tokio::test(flavor = "multi_thread")]
async fn two_worker_processes_share_the_forge_request_budget() {
    let fixture = tempfile::tempdir().unwrap();
    let (base_url, requests, release_forge) = spawn_blocking_forge();
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    for slug in ["acme/one", "acme/two"] {
        control
            .add_repo(yg_control::AddRepo {
                forge_kind: "git",
                base_url: &base_url,
                token_env: None,
                api_root: None,
                slug,
                fetch_depth: None,
                poll_interval_seconds: None,
            })
            .await
            .unwrap();
    }
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    sqlx::query("UPDATE forges SET rate_budget = 1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM jobs")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE repos
         SET last_synced_commit = 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeef'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Worker one spends the only token and blocks inside its Forge request.
    // Its poll loop cannot claim the second repo, forcing worker two to make
    // the competing budget take from another process.
    let _first = spawn_yg_worker(&db_name, |cmd| {
        cmd.env_remove("YG_BOOTSTRAP_TOKEN")
            .env("YG_POLL_INTERVAL", "3600")
            .env("YG_GIT_CACHE", fixture.path().join("git-cache-one"));
    });
    let _second = spawn_yg_worker(&db_name, |cmd| {
        cmd.env_remove("YG_BOOTSTRAP_TOKEN")
            .env("YG_POLL_INTERVAL", "3600")
            .env("YG_GIT_CACHE", fixture.path().join("git-cache-two"));
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let budget_exists =
            sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM forge_rate_budgets)")
                .fetch_one(&pool)
                .await
                .unwrap();
        let due =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM repos WHERE next_poll_at <= now()")
                .fetch_one(&pool)
                .await
                .unwrap();
        let refill_deferred: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM repos
             WHERE next_poll_at BETWEEN now() + interval '35 seconds'
                                    AND now() + interval '65 seconds'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if budget_exists
            && due == 0
            && refill_deferred == 1
            && requests.load(Ordering::Relaxed) == 1
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "both worker processes did not reach the shared poll budget"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let tokens: f64 = sqlx::query_scalar("SELECT tokens FROM forge_rate_budgets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        tokens < 1.0,
        "the shared opening burst must have been charged, got {tokens} tokens"
    );
    assert_eq!(
        requests.load(Ordering::Relaxed),
        1,
        "the second worker must be budget-deferred before reaching the Forge"
    );
    let refill_deferred: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repos
         WHERE next_poll_at BETWEEN now() + interval '35 seconds'
                                AND now() + interval '65 seconds'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        refill_deferred, 1,
        "exactly one of two initial polls should be deferred to the 1/min refill"
    );
    release_forge.store(true, Ordering::Relaxed);
}
