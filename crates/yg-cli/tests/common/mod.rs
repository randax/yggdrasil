//! Shared harness for the end-to-end test targets, run against the dev
//! compose stack — bring it up with the sequence in docs/DEVELOPMENT.md
//! "Checks" (CI runs the same sequence).

// Each test target compiles this module independently and uses its own
// subset of the helpers.
#![allow(dead_code)]

use yg_api::{ObjectStoreConfig, RunningServer, ServerConfig, serve};

pub const DEV_POSTGRES: &str = "postgres://yggdrasil:yggdrasil@localhost:5432";

/// Each test boots against its own freshly created database so tests are
/// independent and re-runnable.
pub async fn boot_test_server() -> RunningServer {
    serve(test_config(&create_test_db().await))
        .await
        .expect("server should boot against the dev stack")
}

/// Postgres CREATE DATABASE statements conflict on the template-database
/// lock when run concurrently; serialize them across parallel tests.
static DB_CREATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn create_test_db() -> String {
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

pub async fn admin_pool() -> sqlx::PgPool {
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

pub fn test_config(db_name: &str) -> ServerConfig {
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

/// The worker-side view of a test database.
pub async fn control_plane(db_name: &str) -> yg_control::ControlPlane {
    yg_control::ControlPlane::connect_and_migrate(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap()
}

/// POST /v1/admin/repos as the test Admin.
pub async fn post_repo(base: &str, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/admin/repos"))
        .bearer_auth("ygt_test_token")
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// GET /v1/admin/status as the test Admin, parsed.
pub async fn admin_status_body(base: &str) -> serde_json::Value {
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
pub fn git(dir: &std::path::Path, args: &[&str]) -> String {
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
pub fn fixture_repo(commits: usize) -> (tempfile::TempDir, std::path::PathBuf, String) {
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
