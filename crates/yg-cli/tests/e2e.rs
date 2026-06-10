//! End-to-end behavior tests, run against the dev compose stack
//! (`docker compose up -d --wait` first; CI does the same).

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
    let admin = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/yggdrasil"))
        .await
        .expect("dev compose Postgres must be up (docker compose up -d --wait)");
    // CREATE DATABASE cannot take bind parameters; the name is generated
    // above from pid + nanos, not external input.
    let _serialize_creates = DB_CREATE_LOCK.lock().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"CREATE DATABASE "{db_name}""#
    )))
    .execute(&admin)
    .await
    .unwrap();
    db_name
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

    let missing = client.get(format!("{base}/v1/status")).send().await.unwrap();
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
