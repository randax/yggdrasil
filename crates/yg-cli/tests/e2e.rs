//! End-to-end behavior tests, run against the dev compose stack — bring
//! it up with the sequence in docs/DEVELOPMENT.md "Checks" (CI runs the
//! same sequence).

use yg_api::{ObjectStoreConfig, RunningServer, ServerConfig, serve};

const DEV_POSTGRES: &str = "postgres://yggdrasil:yggdrasil@localhost:5432";

/// Each test boots against its own freshly created database so tests are
/// independent and re-runnable.
async fn boot_test_server() -> RunningServer {
    serve(test_config(&create_test_db().await))
        .await
        .expect("server should boot against the dev stack")
}

/// Postgres CREATE DATABASE statements conflict on the template-database
/// lock when run concurrently; serialize them across parallel tests.
static DB_CREATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn create_test_db() -> String {
    // pid distinguishes parallel `cargo test` processes; the counter
    // distinguishes parallel tests (the system clock does not — two tests
    // can start within one clock tick); millis distinguish re-runs.
    static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let db_name = format!(
        "yg_test_{}_{}_{}",
        std::process::id(),
        UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let admin = admin_pool().await;
    let _serialize_creates = DB_CREATE_LOCK.lock().await;
    drop_stale_test_dbs(&admin).await;
    // CREATE DATABASE cannot take bind parameters; the name is generated
    // above from pid + counter + millis, not external input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"CREATE DATABASE "{db_name}""#
    )))
    .execute(&admin)
    .await
    .unwrap();
    db_name
}

async fn admin_pool() -> sqlx::PgPool {
    sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/yggdrasil"))
        .await
        .expect("dev compose Postgres must be up (see docs/DEVELOPMENT.md Checks)")
}

/// Best-effort cleanup of databases left behind by earlier runs. Skipped:
/// our own databases (by pid) and anything younger than an hour (by the
/// millis suffix in the name) — a concurrently running `cargo test`
/// process may have created a database it hasn't connected to yet, so
/// "not busy" alone doesn't mean abandoned.
async fn drop_stale_test_dbs(admin: &sqlx::PgPool) {
    let mine = format!("yg_test_{}_", std::process::id());
    let hour_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .saturating_sub(60 * 60 * 1000);
    let candidates: Vec<(String,)> =
        sqlx::query_as("SELECT datname FROM pg_database WHERE datname LIKE 'yg_test_%'")
            .fetch_all(admin)
            .await
            .unwrap_or_default();
    for (name,) in candidates {
        let created_millis: u128 = name
            .rsplit('_')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0); // unparseable = old naming scheme = stale
        if !name.starts_with(&mine) && created_millis < hour_ago {
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(r#"DROP DATABASE "{name}""#)))
                .execute(admin)
                .await;
        }
    }
}

fn test_config(db_name: &str) -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        database_url: format!("{DEV_POSTGRES}/{db_name}"),
        object_store: ObjectStoreConfig {
            endpoint: "http://localhost:9000".into(),
            bucket: "yggdrasil".into(),
            access_key: "yggdrasil".into(),
            secret_key: "yggdrasil".into(),
            region: "us-east-1".into(),
        },
        bootstrap_token: "ygt_test_token".into(),
    }
}

#[tokio::test]
async fn admin_repo_add_registers_repo_and_admin_status_lists_it_queued() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    let add = post_repo(
        &base,
        serde_json::json!({"url": "https://github.com/acme/widgets"}),
    )
    .await;
    assert_eq!(add.status(), 201, "first add must report creation");
    let body: serde_json::Value = add.json().await.unwrap();
    assert_eq!(body["slug"], "acme/widgets");
    assert_eq!(body["created"], true);

    let body = admin_status_body(&base).await;
    let repos = body["repos"].as_array().expect("repos must be a list");
    assert_eq!(repos.len(), 1, "the added repo must be listed, got: {body}");
    assert_eq!(repos[0]["slug"], "acme/widgets");
    assert_eq!(repos[0]["forge"], "https://github.com");
    assert_eq!(
        repos[0]["last_synced_commit"],
        serde_json::Value::Null,
        "nothing synced yet"
    );
    assert_eq!(
        repos[0]["sync"]["state"], "queued",
        "a fetch job must be waiting, got: {body}"
    );
}

#[tokio::test]
async fn admin_repo_add_is_idempotent() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    let first = post_repo(
        &base,
        serde_json::json!({"url": "https://github.com/acme/widgets"}),
    )
    .await;
    assert_eq!(first.status(), 201);
    let body: serde_json::Value = first.json().await.unwrap();
    assert_eq!(body["fetch_queued"], true, "first add queues the fetch");

    // Same repo, cosmetically different URL: trailing slash + .git suffix.
    let again = post_repo(
        &base,
        serde_json::json!({"url": "https://github.com/acme/widgets.git/"}),
    )
    .await;
    assert_eq!(again.status(), 200, "re-add must not be a second creation");
    let body: serde_json::Value = again.json().await.unwrap();
    assert_eq!(body["created"], false);
    assert_eq!(body["slug"], "acme/widgets");
    assert_eq!(
        body["fetch_queued"], false,
        "a fetch is already pending — the re-add must say it queued nothing"
    );

    let body = admin_status_body(&base).await;
    assert_eq!(
        body["repos"].as_array().unwrap().len(),
        1,
        "re-adding must not register a second repo, got: {body}"
    );
}

#[tokio::test]
async fn admin_repo_add_rejects_urls_that_are_not_repositories() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    for url in [
        "not a url",
        "ssh://github.com/acme/widgets", // unsupported scheme
        "https://github.com/acme",       // no repo, just an owner
        "https://github.com",            // no path at all
    ] {
        let resp = post_repo(&base, serde_json::json!({"url": url})).await;
        assert_eq!(resp.status(), 400, "{url:?} must be rejected");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["error"].as_str().is_some_and(|e| !e.is_empty()),
            "rejection must say why, got: {body}"
        );
    }

    let body = admin_status_body(&base).await;
    assert_eq!(
        body["repos"].as_array().unwrap().len(),
        0,
        "rejected URLs must not register anything, got: {body}"
    );
}

/// The worker-side view of a test database.
async fn control_plane(db_name: &str) -> yg_control::ControlPlane {
    yg_control::ControlPlane::connect_and_migrate(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap()
}

/// POST /v1/admin/repos as the test Admin.
async fn post_repo(base: &str, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/admin/repos"))
        .bearer_auth("ygt_test_token")
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// GET /v1/admin/status as the test Admin, parsed.
async fn admin_status_body(base: &str) -> serde_json::Value {
    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/admin/status"))
        .bearer_auth("ygt_test_token")
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "admin status returned {status}: {text}"
    );
    serde_json::from_str(&text).expect("admin status must be JSON")
}

/// Run git in a directory, panicking (with stderr) on failure.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git must be installed to run the sync tests");
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A local git repository standing in for a Forge-hosted one, addressable
/// as `file://…/acme/widgets`. Returns (fixture root guard, repo dir, URL).
fn fixture_repo(commits: usize) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("fixtures/acme/widgets");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "fixture@example.com"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    // A developer's global commit.gpgsign=true would make fixture
    // commits demand a signing key; fixtures must not depend on the
    // machine's git config.
    git(&repo, &["config", "commit.gpgsign", "false"]);
    for n in 1..=commits {
        std::fs::write(repo.join("README.md"), format!("revision {n}\n")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", &format!("commit {n}")]);
    }
    let url = format!("file://{}", repo.display());
    (root, repo, url)
}

#[tokio::test]
async fn worker_syncs_added_repo_and_status_shows_its_commit() {
    let (fixture, repo_dir, fixture_url) = fixture_repo(1);
    let head = git(&repo_dir, &["rev-parse", "HEAD"]);

    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");
    let base = format!("http://{}", server.local_addr());

    let add = post_repo(&base, serde_json::json!({"url": fixture_url})).await;
    assert_eq!(add.status(), 201);

    let control = control_plane(&db_name).await;
    let worker = yg_sync::SyncWorker::new(control, fixture.path().join("git-cache"));
    let worked = worker.run_once().await.expect("sync must not error");
    assert!(worked, "the queued fetch job must be picked up");

    let body = admin_status_body(&base).await;
    assert_eq!(
        body["repos"][0]["last_synced_commit"].as_str(),
        Some(head.as_str()),
        "status must show the fixture's HEAD, got: {body}"
    );
    assert_eq!(body["repos"][0]["sync"]["state"], "synced");

    assert!(
        !worker.run_once().await.expect("idle poll must not error"),
        "queue must be drained after the one job"
    );
}

#[tokio::test]
async fn re_adding_a_synced_repo_queues_a_fresh_fetch_that_picks_up_new_commits() {
    let (fixture, repo_dir, fixture_url) = fixture_repo(1);

    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");
    let base = format!("http://{}", server.local_addr());
    let control = control_plane(&db_name).await;
    let worker = yg_sync::SyncWorker::new(control, fixture.path().join("git-cache"));

    let add = || post_repo(&base, serde_json::json!({"url": fixture_url}));
    let synced_commit = || async {
        admin_status_body(&base).await["repos"][0]["last_synced_commit"]
            .as_str()
            .map(str::to_string)
    };

    add().await;
    assert!(worker.run_once().await.unwrap());
    let first_head = git(&repo_dir, &["rev-parse", "HEAD"]);
    assert_eq!(synced_commit().await.as_deref(), Some(first_head.as_str()));

    // The repo moves on the forge; re-adding it requests a fresh sync.
    std::fs::write(repo_dir.join("README.md"), "revision 2\n").unwrap();
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "commit 2"]);
    let second_head = git(&repo_dir, &["rev-parse", "HEAD"]);

    let re_add = add().await;
    let body: serde_json::Value = re_add.json().await.unwrap();
    assert_eq!(
        body["fetch_queued"], true,
        "with the previous fetch done, a re-add queues a fresh one"
    );
    assert!(
        worker.run_once().await.unwrap(),
        "re-add must queue another fetch for the synced repo"
    );
    assert_eq!(
        synced_commit().await.as_deref(),
        Some(second_head.as_str()),
        "the fetch must advance the synced commit"
    );
}

#[tokio::test]
async fn a_vandalized_cache_mirror_heals_on_the_next_sync() {
    let (fixture, repo_dir, fixture_url) = fixture_repo(2);
    let head = git(&repo_dir, &["rev-parse", "HEAD"]);

    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");
    let base = format!("http://{}", server.local_addr());
    let control = control_plane(&db_name).await;
    let cache = fixture.path().join("git-cache");
    let worker = yg_sync::SyncWorker::new(control, &cache);

    post_repo(&base, serde_json::json!({"url": fixture_url})).await;
    assert!(worker.run_once().await.unwrap());

    // A crashed clone, a stray rm, a partial disk: the mirror is junk now.
    let mirror = std::fs::read_dir(&cache)
        .unwrap()
        .next()
        .expect("mirror must exist")
        .unwrap()
        .path();
    std::fs::remove_dir_all(&mirror).unwrap();
    std::fs::create_dir_all(mirror.join("not-a-git-repo")).unwrap();

    post_repo(&base, serde_json::json!({"url": fixture_url})).await;
    assert!(worker.run_once().await.unwrap());

    let body = admin_status_body(&base).await;
    assert_eq!(
        body["repos"][0]["sync"]["state"], "synced",
        "the worker must re-clone over an unusable mirror, got: {body}"
    );
    assert_eq!(
        body["repos"][0]["last_synced_commit"].as_str(),
        Some(head.as_str())
    );

    // Worse: the mirror path is now a plain file, not even a directory.
    std::fs::remove_dir_all(&mirror).unwrap();
    std::fs::write(&mirror, "wreckage").unwrap();

    post_repo(&base, serde_json::json!({"url": fixture_url})).await;
    assert!(worker.run_once().await.unwrap());

    let body = admin_status_body(&base).await;
    assert_eq!(
        body["repos"][0]["sync"]["state"], "synced",
        "the worker must re-clone over a file squatting on the mirror path, got: {body}"
    );
}

#[tokio::test]
async fn re_adding_heals_a_forge_row_missing_its_token_env() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let add = || {
        control.add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: Some("YG_GITHUB_TOKEN"),
            slug: "acme/widgets",
            fetch_depth: None,
        })
    };
    add().await.unwrap();

    // A degraded forge row — manual insert, older deployment.
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    sqlx::query("UPDATE forges SET token_env = NULL")
        .execute(&pool)
        .await
        .unwrap();

    add().await.unwrap();
    let job = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("fetch job claimable");
    assert_eq!(
        job.token_env.as_deref(),
        Some("YG_GITHUB_TOKEN"),
        "re-adding must backfill a missing token_env"
    );
}

#[tokio::test]
async fn stale_partial_clones_are_swept_on_the_next_sync() {
    let (fixture, _repo_dir, fixture_url) = fixture_repo(1);

    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");
    let base = format!("http://{}", server.local_addr());
    let control = control_plane(&db_name).await;
    let cache = fixture.path().join("git-cache");
    let worker = yg_sync::SyncWorker::new(control, &cache);

    post_repo(&base, serde_json::json!({"url": fixture_url})).await;
    assert!(worker.run_once().await.unwrap());

    // Wreckage from a crashed clone attempt sits beside the mirror.
    let mirror_name = std::fs::read_dir(&cache)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let stale = cache.join(format!("{}.partial.4242-7", mirror_name.to_string_lossy()));
    std::fs::create_dir_all(stale.join("objects")).unwrap();

    post_repo(&base, serde_json::json!({"url": fixture_url})).await;
    assert!(worker.run_once().await.unwrap());

    let leftovers: Vec<String> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("partial"))
        .collect();
    assert_eq!(
        leftovers,
        Vec::<String>::new(),
        "syncing must sweep crashed clone attempts"
    );
    assert_eq!(
        admin_status_body(&base).await["repos"][0]["sync"]["state"],
        "synced"
    );
}

#[tokio::test]
async fn a_fetch_job_outlives_its_crashed_worker_via_lease_expiry() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: Some("YG_GITHUB_TOKEN"),
            slug: "acme/widgets",
            fetch_depth: None,
        })
        .await
        .unwrap();

    // A worker claims the job and crashes: its lease expires instantly.
    let crashed = control
        .claim_due_fetch(std::time::Duration::ZERO)
        .await
        .unwrap()
        .expect("the queued job must be claimable");

    // Another worker picks the same job up once the lease is gone…
    let recovered = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("an expired lease must make the job claimable again");
    assert_eq!(recovered.job_id, crashed.job_id, "same job, not a copy");
    assert_eq!(recovered.attempts, 0, "a crash is not a fetch failure");

    // …and while that lease is live, nobody else can.
    assert!(
        control
            .claim_due_fetch(std::time::Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "a live lease must block other workers"
    );
}

#[tokio::test]
async fn a_worker_that_outlived_its_lease_cannot_clobber_the_new_claim() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: Some("YG_GITHUB_TOKEN"),
            slug: "acme/widgets",
            fetch_depth: None,
        })
        .await
        .unwrap();

    // Worker A stalls long enough for its lease to lapse; worker B takes
    // over the job.
    let stale = control
        .claim_due_fetch(std::time::Duration::ZERO)
        .await
        .unwrap()
        .expect("claimable");
    let fresh = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("expired lease must be claimable");

    // A finally finishes — too late. Its result must be discarded.
    assert!(
        !control
            .complete_fetch(&stale, "deadbeef0000000000000000000000000000dead")
            .await
            .unwrap(),
        "a stale completion must report that it was discarded"
    );
    let repos = control.admin_status().await.unwrap();
    assert_eq!(
        repos[0].last_synced_commit, None,
        "a stale completion must not advance the synced commit"
    );
    assert_eq!(
        repos[0].job_state.as_deref(),
        Some("leased"),
        "the job must still belong to worker B"
    );

    // A stale failure must not reset B's job either.
    assert!(
        !control.fail_fetch(&stale, "boom").await.unwrap(),
        "a stale failure must report that it was discarded"
    );
    let repos = control.admin_status().await.unwrap();
    assert_eq!(repos[0].attempts, 0, "stale failure must not count");
    assert_eq!(repos[0].job_state.as_deref(), Some("leased"));

    // B's own completion still lands.
    assert!(
        control
            .complete_fetch(&fresh, "feedface0000000000000000000000000000feed")
            .await
            .unwrap(),
        "the live lease holder's completion must apply"
    );
    let repos = control.admin_status().await.unwrap();
    assert_eq!(
        repos[0].last_synced_commit.as_deref(),
        Some("feedface0000000000000000000000000000feed")
    );
}

#[tokio::test]
async fn admin_repo_add_rejects_non_positive_depth() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    for depth in [0, -3] {
        let resp = post_repo(
            &base,
            serde_json::json!({"url": "https://github.com/acme/widgets", "depth": depth}),
        )
        .await;
        assert_eq!(resp.status(), 400, "depth {depth} must be rejected");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["error"].as_str().is_some_and(|e| e.contains("depth")),
            "the error must name depth, got: {body}"
        );
    }

    let body = admin_status_body(&base).await;
    assert_eq!(
        body["repos"].as_array().unwrap().len(),
        0,
        "a rejected depth must not register the repo, got: {body}"
    );

    // The CLI rejects it before the request even leaves.
    let cli = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &base)
        .env("YG_TOKEN", "ygt_test_token")
        .args([
            "admin",
            "repo",
            "add",
            "https://github.com/acme/widgets",
            "--depth",
            "0",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8(cli.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("depth") || stderr.contains("--depth"),
        "clap must reject depth 0, got:\n{stderr}"
    );
}

#[tokio::test]
async fn failing_fetches_surface_their_error_and_back_off_exponentially() {
    let fixture = tempfile::tempdir().unwrap();
    // Valid URL shape, but nothing lives there.
    let bad_url = format!("file://{}/gone/acme/widgets", fixture.path().display());

    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");
    let base = format!("http://{}", server.local_addr());
    let db_url = format!("{DEV_POSTGRES}/{db_name}");
    let control = control_plane(&db_name).await;
    let worker = yg_sync::SyncWorker::new(control, fixture.path().join("git-cache"));

    post_repo(&base, serde_json::json!({"url": bad_url})).await;
    assert!(
        worker
            .run_once()
            .await
            .expect("a failed fetch is handled, not an error"),
        "the job must still be claimed"
    );

    let body = admin_status_body(&base).await;
    let sync = &body["repos"][0]["sync"];
    assert_eq!(sync["state"], "retrying", "got: {body}");
    assert_eq!(sync["attempts"], 1);
    assert!(
        sync["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("clon")),
        "the error must say what failed, got: {body}"
    );

    let control = control_plane(&db_name).await;
    assert!(
        control
            .claim_due_fetch(std::time::Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "a failed job must not be due again immediately"
    );

    // Backoff must grow: time-travel the job back to due, fail it again,
    // and compare the scheduled delays.
    let pool = sqlx::PgPool::connect(&db_url).await.unwrap();
    let first_delay: f64 = delay_seconds(&pool).await;
    sqlx::query("UPDATE jobs SET run_after = now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        worker.run_once().await.unwrap(),
        "due again after time travel"
    );
    let second_delay: f64 = delay_seconds(&pool).await;
    assert!(
        second_delay > first_delay * 1.5,
        "backoff must grow per failure: first {first_delay}s, second {second_delay}s"
    );
}

/// Seconds until the single queued job is due again.
async fn delay_seconds(pool: &sqlx::PgPool) -> f64 {
    let (delay,): (f64,) =
        sqlx::query_as("SELECT extract(epoch FROM run_after - now())::float8 FROM jobs")
            .fetch_one(pool)
            .await
            .unwrap();
    delay
}

#[tokio::test]
async fn depth_override_clones_shallow_while_default_keeps_full_history() {
    let (fixture, repo_dir, fixture_url) = fixture_repo(3);
    let head = git(&repo_dir, &["rev-parse", "HEAD"]);

    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");
    let base = format!("http://{}", server.local_addr());
    let control = control_plane(&db_name).await;
    let cache = fixture.path().join("git-cache");
    let worker = yg_sync::SyncWorker::new(control, &cache);

    let commit_count_in_cache = |cache: std::path::PathBuf| {
        // The cache holds one bare mirror per synced repo; with a single
        // repo added, the single entry is it.
        let mirror = std::fs::read_dir(&cache)
            .unwrap()
            .next()
            .expect("the worker must have cloned into the cache")
            .unwrap()
            .path();
        git(&mirror, &["rev-list", "--count", "HEAD"])
    };

    post_repo(&base, serde_json::json!({"url": fixture_url, "depth": 1})).await;
    assert!(worker.run_once().await.unwrap());

    let body = admin_status_body(&base).await;
    assert_eq!(
        body["repos"][0]["last_synced_commit"].as_str(),
        Some(head.as_str()),
        "shallow still syncs the tip, got: {body}"
    );
    assert_eq!(
        commit_count_in_cache(cache.clone()),
        "1",
        "depth=1 must clone only the tip commit"
    );

    // The same fixture without an override mirrors all of history.
    let (fixture_full, _full_repo_dir, full_url) = fixture_repo(3);
    let full_cache = fixture_full.path().join("git-cache");
    let control = control_plane(&db_name).await;
    let full_worker = yg_sync::SyncWorker::new(control, &full_cache);
    post_repo(&base, serde_json::json!({"url": full_url})).await;
    assert!(full_worker.run_once().await.unwrap());
    assert_eq!(
        commit_count_in_cache(full_cache),
        "3",
        "no override must fetch full history"
    );
}

#[tokio::test]
async fn removing_the_depth_override_restores_full_history() {
    let (fixture, _repo_dir, fixture_url) = fixture_repo(3);

    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");
    let base = format!("http://{}", server.local_addr());
    let control = control_plane(&db_name).await;
    let cache = fixture.path().join("git-cache");
    let worker = yg_sync::SyncWorker::new(control, &cache);

    post_repo(&base, serde_json::json!({"url": fixture_url, "depth": 1})).await;
    assert!(worker.run_once().await.unwrap());

    let mirror = std::fs::read_dir(&cache)
        .unwrap()
        .next()
        .expect("mirror must exist")
        .unwrap()
        .path();
    assert_eq!(
        git(&mirror, &["rev-list", "--count", "HEAD"]),
        "1",
        "the override starts the mirror shallow"
    );

    // Dropping the override goes back to the default: full history.
    post_repo(&base, serde_json::json!({"url": fixture_url})).await;
    assert!(worker.run_once().await.unwrap());
    assert_eq!(
        git(&mirror, &["rev-list", "--count", "HEAD"]),
        "3",
        "without the override the mirror must deepen to full history"
    );
}

/// multi_thread: the yg binary blocks this thread while the in-process
/// server it queries runs on the same test runtime.
#[tokio::test(flavor = "multi_thread")]
async fn yg_admin_repo_add_and_admin_status_drive_the_admin_surface() {
    let server = boot_test_server().await;
    let env = [
        ("YG_SERVER", format!("http://{}", server.local_addr())),
        ("YG_TOKEN", "ygt_test_token".into()),
    ];

    let add = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .envs(env.iter().cloned())
        .args(["admin", "repo", "add", "https://github.com/acme/widgets"])
        .assert()
        .success();
    let stdout = String::from_utf8(add.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("registered") && stdout.contains("acme/widgets"),
        "add must confirm what it did, got:\n{stdout}"
    );

    let re_add = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .envs(env.iter().cloned())
        .args(["admin", "repo", "add", "https://github.com/acme/widgets"])
        .assert()
        .success();
    let stdout = String::from_utf8(re_add.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("already registered"),
        "re-add must say the repo was known, got:\n{stdout}"
    );
    assert!(
        stdout.contains("already pending"),
        "re-add must not claim it queued a fetch when one is pending, got:\n{stdout}"
    );

    let status = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .envs(env.iter().cloned())
        .args(["admin", "status"])
        .assert()
        .success();
    let stdout = String::from_utf8(status.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("acme/widgets") && stdout.contains("queued"),
        "status must list the repo with its sync state, got:\n{stdout}"
    );

    let json = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .envs(env.iter().cloned())
        .args(["admin", "status", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8(json.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json output must be valid JSON");
    assert_eq!(body["repos"][0]["slug"], "acme/widgets");
    assert_eq!(body["repos"][0]["sync"]["state"], "queued");

    let rejected = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .envs(env.iter().cloned())
        .args(["admin", "repo", "add", "https://github.com/just-an-owner"])
        .assert()
        .failure();
    let stderr = String::from_utf8(rejected.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("owner/repo"),
        "a rejected URL must explain itself, got:\n{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn yg_serve_role_all_syncs_an_added_repo_without_a_separate_worker() {
    use std::io::BufRead;

    let (fixture, repo_dir, fixture_url) = fixture_repo(1);
    let head = git(&repo_dir, &["rev-parse", "HEAD"]);

    let db_name = create_test_db().await;
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("yg"))
        .env("YG_LISTEN", "127.0.0.1:0")
        .env("YG_DATABASE_URL", format!("{DEV_POSTGRES}/{db_name}"))
        .env("YG_BOOTSTRAP_TOKEN", "ygt_test_token")
        .env("YG_GIT_CACHE", fixture.path().join("git-cache"))
        .args(["serve", "--role=all"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut server = KillOnDrop(child);
    let stdout = server.0.stdout.take().unwrap();
    let first_line = std::io::BufReader::new(stdout)
        .lines()
        .next()
        .expect("yg serve must announce its address")
        .unwrap();
    let url = first_line
        .strip_prefix("listening on ")
        .unwrap()
        .to_string();

    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &url)
        .env("YG_TOKEN", "ygt_test_token")
        .args(["admin", "repo", "add", &fixture_url])
        .assert()
        .success();

    // The in-process worker picks the job up on its own; no worker
    // process, no manual nudge.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let body = admin_status_body(&url).await;
        if body["repos"][0]["last_synced_commit"].as_str() == Some(head.as_str()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "repo never synced; last status: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn yg_serve_role_worker_drains_the_queue_without_serving_http() {
    let (fixture, repo_dir, fixture_url) = fixture_repo(1);
    let head = git(&repo_dir, &["rev-parse", "HEAD"]);

    let db_name = create_test_db().await;
    let db_url = format!("{DEV_POSTGRES}/{db_name}");
    let control = control_plane(&db_name).await;
    let locator = fixture_url.strip_prefix("file://").unwrap();
    let (base, slug) = locator.rsplit_once("/acme/").unwrap();
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "git",
            base_url: &format!("file://{base}"),
            token_env: None,
            slug: &format!("acme/{slug}"),
            fetch_depth: None,
        })
        .await
        .unwrap();

    // Worker role: no HTTP, no bootstrap token — just the queue.
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("yg"))
        .env_remove("YG_BOOTSTRAP_TOKEN")
        .env("YG_DATABASE_URL", &db_url)
        .env("YG_GIT_CACHE", fixture.path().join("git-cache"))
        .args(["serve", "--role=worker"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _worker = KillOnDrop(child);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let repos = control.admin_status().await.unwrap();
        if repos[0].last_synced_commit.as_deref() == Some(head.as_str()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker never synced the repo; last state: {:?} after {:?} attempts",
            repos[0].job_state,
            repos[0].attempts
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn requests_without_a_valid_token_get_401_except_health() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    let missing = client
        .get(format!("{base}/v1/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401, "missing token must be rejected");

    let wrong = client
        .get(format!("{base}/v1/status"))
        .bearer_auth("ygt_definitely_wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401, "invalid token must be rejected");

    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200, "health must not require a token");

    // "Every route except health" includes paths that don't exist —
    // unauthenticated clients must not be able to enumerate the API.
    for path in ["/", "/v1/", "/v1/nonexistent"] {
        let resp = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(resp.status(), 401, "unauthenticated {path} must get 401");
    }
    let authed_unknown = client
        .get(format!("{base}/v1/nonexistent"))
        .bearer_auth("ygt_test_token")
        .send()
        .await
        .unwrap();
    assert_eq!(authed_unknown.status(), 404, "valid token sees real 404s");

    // RFC 9110: the auth scheme is case-insensitive.
    let lowercase_scheme = client
        .get(format!("{base}/v1/status"))
        .header("Authorization", "bearer ygt_test_token")
        .send()
        .await
        .unwrap();
    assert_eq!(
        lowercase_scheme.status(),
        200,
        "lowercase scheme with a valid token must be accepted"
    );

    // RFC 9110 allows one *or more* spaces between scheme and credentials.
    let double_space = client
        .get(format!("{base}/v1/status"))
        .header("Authorization", "Bearer  ygt_test_token")
        .send()
        .await
        .unwrap();
    assert_eq!(
        double_space.status(),
        200,
        "multiple spaces after the scheme are legal"
    );
}

#[test]
fn yg_serve_refuses_to_boot_with_an_empty_bootstrap_token() {
    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_BOOTSTRAP_TOKEN", "")
        .env("YG_LISTEN", "127.0.0.1:0")
        // Unreachable on purpose: the token must be rejected before any
        // connection is attempted, so this must never be dialed.
        .env("YG_DATABASE_URL", "postgres://nobody@127.0.0.1:1/none")
        .timeout(std::time::Duration::from_secs(20))
        .arg("serve")
        .assert()
        .failure()
        .stderr(predicates::str::contains("YG_BOOTSTRAP_TOKEN"));
}

#[tokio::test]
async fn status_reports_version_uptime_and_repo_count_to_a_valid_token() {
    let server = boot_test_server().await;

    let resp = reqwest::Client::new()
        .get(format!("http://{}/v1/status", server.local_addr()))
        .bearer_auth("ygt_test_token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["repos_indexed"], 0, "fresh deployment indexes nothing");
    assert!(
        body["uptime_seconds"].is_u64(),
        "uptime must be a number, got: {body}"
    );
}

#[tokio::test]
async fn migrations_are_idempotent_across_server_restarts() {
    let db_name = create_test_db().await;

    let first = serve(test_config(&db_name)).await.expect("first boot");
    drop(first);

    let second = serve(test_config(&db_name))
        .await
        .expect("restart against an already-migrated database");

    let resp = reqwest::Client::new()
        .get(format!("http://{}/v1/status", second.local_addr()))
        .bearer_auth("ygt_test_token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "restarted server must serve status");
}

/// multi_thread: the yg binary blocks this thread while the in-process
/// server it queries runs on the same test runtime.
#[tokio::test(flavor = "multi_thread")]
async fn yg_status_prints_a_human_readable_report() {
    let server = boot_test_server().await;

    let assert = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", format!("http://{}", server.local_addr()))
        .env("YG_TOKEN", "ygt_test_token")
        .arg("status")
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "must show server version, got:\n{stdout}"
    );
    assert!(
        stdout.contains("repos indexed: 0"),
        "must show indexed-repo count, got:\n{stdout}"
    );
    assert!(
        stdout.contains("uptime:"),
        "must show uptime, got:\n{stdout}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn yg_status_json_emits_machine_readable_output() {
    let server = boot_test_server().await;

    let assert = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        // Trailing slash on purpose: the CLI must not build a `//v1/…` URL.
        .env("YG_SERVER", format!("http://{}/", server.local_addr()))
        .env("YG_TOKEN", "ygt_test_token")
        .args(["status", "--json"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json output must be valid JSON");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["repos_indexed"], 0);
    assert!(body["uptime_seconds"].is_u64());
}

#[tokio::test]
async fn health_degrades_to_503_when_a_dependency_dies() {
    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");

    // Sever the control plane out from under the running server.
    let admin = admin_pool().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"DROP DATABASE "{db_name}" WITH (FORCE)"#
    )))
    .execute(&admin)
    .await
    .unwrap();

    let resp = reqwest::get(format!("http://{}/healthz", server.local_addr()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "lost dependency must degrade health");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "degraded");
    assert_eq!(
        body["checks"]["object_store"], "ok",
        "storage is still fine"
    );
    assert!(
        body["checks"]["postgres"]
            .as_str()
            .unwrap()
            .starts_with("error"),
        "postgres check must carry the failure, got: {body}"
    );
}

/// Kills the spawned server even when the test panics.
struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn yg_serve_boots_from_env_and_answers_yg_status_end_to_end() {
    use std::io::BufRead;

    let db_name = create_test_db().await;
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("yg"))
        .env("YG_LISTEN", "127.0.0.1:0")
        .env("YG_DATABASE_URL", format!("{DEV_POSTGRES}/{db_name}"))
        // Padded on purpose: env files commonly leak whitespace around
        // secrets; clients present the clean token below.
        .env("YG_BOOTSTRAP_TOKEN", " ygt_test_token\n")
        .args(["serve", "--role=all"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = KillOnDrop(child);

    let stdout = server.0.stdout.take().unwrap();
    let first_line = match std::io::BufReader::new(stdout).lines().next() {
        Some(line) => line.unwrap(),
        None => {
            use std::io::Read;
            let _ = server.0.kill();
            let mut stderr = String::new();
            if let Some(mut err) = server.0.stderr.take() {
                let _ = err.read_to_string(&mut stderr);
            }
            panic!("yg serve exited without announcing its address; stderr:\n{stderr}");
        }
    };
    let url = first_line
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("unexpected announcement: {first_line}"))
        .to_string();

    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &url)
        .env("YG_TOKEN", "ygt_test_token")
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("repos indexed: 0"));
}

#[tokio::test]
async fn server_boots_and_health_reports_server_and_storage_readiness() {
    let server = boot_test_server().await;

    let resp = reqwest::get(format!("http://{}/healthz", server.local_addr()))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["checks"]["postgres"], "ok");
    assert_eq!(body["checks"]["object_store"], "ok");
}
