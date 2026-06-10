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
        .env("YG_SERVER", format!("http://{}", server.local_addr()))
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
        .env("YG_BOOTSTRAP_TOKEN", "ygt_test_token")
        .args(["serve", "--role=all"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = KillOnDrop(child);

    let stdout = server.0.stdout.take().unwrap();
    let first_line = std::io::BufReader::new(stdout)
        .lines()
        .next()
        .expect("yg serve must announce its address before exiting")
        .unwrap();
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
