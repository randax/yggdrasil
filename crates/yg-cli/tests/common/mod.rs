//! Shared harness for the end-to-end test targets, run against the dev
//! compose stack — bring it up with the sequence in docs/DEVELOPMENT.md
//! "Checks" (CI runs the same sequence).

// Each test target compiles this module independently and uses its own
// subset of the helpers.
#![allow(dead_code)]

use yg_api::{ObjectStoreConfig, RunningServer, ServerConfig, serve};

/// The bearer token every harness server boots with and every helper
/// presents.
pub const TEST_TOKEN: &str = "ygt_test_token";

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
    sweep_stale_object_prefixes(&admin).await;
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

/// Best-effort cleanup of object-store key prefixes left behind by
/// earlier runs, so the shared dev bucket doesn't grow forever. Same
/// staleness rules as [`drop_stale_test_dbs`] (skip this process's
/// prefixes and anything younger than an hour), plus one more guard:
/// skip any prefix whose database still exists — its run may be live,
/// and the next sweep gets it once the database is gone.
async fn sweep_stale_object_prefixes(admin: &sqlx::PgPool) {
    use futures::{StreamExt, TryStreamExt};
    let mine = format!("yg_test_{}_", std::process::id());
    let hour_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .saturating_sub(60 * 60 * 1000);
    let live: std::collections::HashSet<String> =
        sqlx::query_as("SELECT datname FROM pg_database WHERE datname LIKE 'yg_test_%'")
            .fetch_all(admin)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(name,): (String,)| name)
            .collect();
    let store = dev_object_store("");
    let Ok(root) = store.list_with_delimiter(None).await else {
        return;
    };
    for prefix in root.common_prefixes {
        let name = prefix.as_ref();
        let created_millis: u128 = name
            .rsplit('_')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0); // unparseable = old naming scheme = stale
        if !name.starts_with("yg_test_")
            || name.starts_with(&mine)
            || live.contains(name)
            || created_millis >= hour_ago
        {
            continue;
        }
        let locations = store.list(Some(&prefix)).map_ok(|o| o.location).boxed();
        let _ = store
            .delete_stream(locations)
            .try_collect::<Vec<_>>()
            .await;
    }
}

pub fn test_config(db_name: &str) -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        database_url: format!("{DEV_POSTGRES}/{db_name}"),
        // Every test database gets its own key namespace in the
        // shared dev bucket, so identical fixture commits across
        // parallel tests can never read each other's Shards.
        object_store: dev_object_store_config(db_name),
        bootstrap_token: TEST_TOKEN.into(),
        // Per-test (db names are unique) so cache assertions never see
        // another test's segments; content-addressing would make sharing
        // safe, but isolation keeps the tests honest.
        shard_cache: std::env::temp_dir().join(format!("yg-shard-cache-{db_name}")),
    }
}

/// The dev compose stack's MinIO, keyed under `key_prefix` (empty
/// means the bucket root).
pub fn dev_object_store_config(key_prefix: &str) -> ObjectStoreConfig {
    ObjectStoreConfig {
        endpoint: "http://localhost:9000".into(),
        bucket: "yggdrasil".into(),
        access_key: "yggdrasil".into(),
        secret_key: "yggdrasil".into(),
        region: "us-east-1".into(),
        key_prefix: key_prefix.into(),
    }
}

/// [`dev_object_store_config`], connected.
pub fn dev_object_store(key_prefix: &str) -> std::sync::Arc<dyn object_store::ObjectStore> {
    dev_object_store_config(key_prefix).connect().unwrap()
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
        .bearer_auth(TEST_TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// GET /v1/admin/status as the test Admin, parsed.
pub async fn admin_status_body(base: &str) -> serde_json::Value {
    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/admin/status"))
        .bearer_auth(TEST_TOKEN)
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

/// The Symbol names the `search` Verb returns for `query`, against a
/// server reached over HTTP (the spawned-process e2e demos).
pub async fn search_hit_names(base: &str, query: &str) -> Vec<String> {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/verbs/search"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({"query": query}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    body["hits"]
        .as_array()
        .map(|hits| {
            hits.iter()
                .filter_map(|hit| hit["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Poll the running server until `name` is queryable, or fail at the
/// deadline — the observable "the Knowledge Graph caught up" check.
pub async fn await_symbol(base: &str, name: &str, within: std::time::Duration) {
    let deadline = std::time::Instant::now() + within;
    loop {
        if search_hit_names(base, name).await.iter().any(|n| n == name) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "symbol {name:?} never became queryable within {within:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Kills a spawned server process even when the test panics.
pub struct KillOnDrop(pub std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `yg serve --role=<role>` as a real process wired to the test
/// database (its Postgres URL and its object-store key prefix), wait
/// for the readiness line on stdout, and return the kill guard plus
/// that line. `configure` adds the test's environment on top of
/// `YG_LISTEN=127.0.0.1:0`. A process that dies before announcing
/// panics with its stderr instead of an opaque unwrap; a live one has
/// its stderr drained on a thread so a chatty run can never fill the
/// pipe and block.
pub fn spawn_yg_role(
    role: &str,
    db_name: &str,
    configure: impl FnOnce(&mut std::process::Command),
) -> (KillOnDrop, String) {
    use std::io::{BufRead, Read};
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("yg"));
    cmd.env("YG_LISTEN", "127.0.0.1:0")
        .env("YG_DATABASE_URL", format!("{DEV_POSTGRES}/{db_name}"))
        .env("YG_S3_PREFIX", db_name);
    configure(&mut cmd);
    let child = cmd
        .args(["serve", &format!("--role={role}")])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut server = KillOnDrop(child);
    let mut stderr = server.0.stderr.take().unwrap();
    let stderr_drain = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    let stdout = server.0.stdout.take().unwrap();
    let first_line = match std::io::BufReader::new(stdout).lines().next() {
        Some(line) => line.unwrap(),
        None => {
            let _ = server.0.kill();
            let stderr = stderr_drain.join().unwrap_or_default();
            panic!("yg serve --role={role} exited without announcing readiness; stderr:\n{stderr}");
        }
    };
    (server, first_line)
}

/// [`spawn_yg_role`] for the combined role, returning the announced
/// base URL.
pub fn spawn_yg_serve(
    db_name: &str,
    configure: impl FnOnce(&mut std::process::Command),
) -> (KillOnDrop, String) {
    let (server, first_line) = spawn_yg_role("all", db_name, configure);
    let url = base_url(&first_line);
    (server, url)
}

/// [`spawn_yg_role`] for an HTTP-serving role, returning the announced
/// base URL.
pub fn spawn_yg_api(
    db_name: &str,
    configure: impl FnOnce(&mut std::process::Command),
) -> (KillOnDrop, String) {
    let (server, first_line) = spawn_yg_role("api", db_name, configure);
    let url = base_url(&first_line);
    (server, url)
}

/// [`spawn_yg_role`] for the worker role, asserting its readiness
/// announcement (workers serve no HTTP, so there is no URL to return).
pub fn spawn_yg_worker(
    db_name: &str,
    configure: impl FnOnce(&mut std::process::Command),
) -> KillOnDrop {
    let (worker, announcement) = spawn_yg_role("worker", db_name, configure);
    assert_eq!(announcement, "worker running");
    worker
}

fn base_url(announcement: &str) -> String {
    announcement
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("unexpected announcement: {announcement}"))
        .to_string()
}

/// The one repo mirror inside a worker's git cache. The cache root
/// holds a `<id>.git` dir per synced repo beside lock files and,
/// mid-clone, partial dirs — and these tests sync exactly one repo.
pub fn only_mirror(cache: &std::path::Path) -> std::path::PathBuf {
    let mirrors: Vec<_> = std::fs::read_dir(cache)
        .expect("the worker must have created its git cache")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir() && path.extension().is_some_and(|ext| ext == "git"))
        .collect();
    match mirrors.as_slice() {
        [mirror] => mirror.clone(),
        other => panic!(
            "expected exactly one mirror in {}, found {other:?}",
            cache.display()
        ),
    }
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

/// Like [`git`], but with extra environment — the author/committer
/// identity and dates a history fixture needs so its commits have
/// deterministic, machine-independent timestamps.
pub fn git_env(dir: &std::path::Path, envs: &[(&str, &str)], args: &[&str]) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = cmd
        .output()
        .expect("git must be installed to run the history tests");
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// One commit in a history fixture: who wrote it, when (unix seconds), its
/// message, and the files it writes.
pub struct FixtureCommit<'a> {
    pub author: &'a str,
    pub email: &'a str,
    pub when: i64,
    pub message: &'a str,
    pub files: &'a [(&'a str, &'a str)],
}

/// A fixture repo with an explicit commit history, applied in order (so
/// the last entry is newest). Each commit's author and timestamp are
/// fixed, giving history tests a deterministic newest-first order and
/// stable authorship. Returns (fixture root guard, repo dir, URL).
pub fn history_fixture(
    commits: &[FixtureCommit<'_>],
) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let (root, repo, url) = empty_fixture_repo();
    for commit in commits {
        for (path, contents) in commit.files {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
        git(&repo, &["add", "."]);
        let date = format!("{} +0000", commit.when);
        git_env(
            &repo,
            &[
                ("GIT_AUTHOR_NAME", commit.author),
                ("GIT_AUTHOR_EMAIL", commit.email),
                ("GIT_AUTHOR_DATE", &date),
                ("GIT_COMMITTER_NAME", commit.author),
                ("GIT_COMMITTER_EMAIL", commit.email),
                ("GIT_COMMITTER_DATE", &date),
            ],
            &["commit", "-m", commit.message],
        );
    }
    (root, repo, url)
}

/// A Go repo fixture standing in for a Forge-hosted one: two functions in
/// main.go plus a README, giving the graph File and Symbol nodes joined
/// by DEFINES edges.
pub fn go_fixture_repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
    fixture_repo_with(&[
        (
            "main.go",
            "package main\n\nfunc Hello() string {\n\treturn \"hello\"\n}\n\nfunc main() {\n\tprintln(Hello())\n}\n",
        ),
        ("README.md", "# gadgets\n"),
    ])
}

/// A booted server plus sync and index workers around one fixture repo:
/// everything an indexing or Verb test drives. Per-target extras live in
/// `impl Harness` blocks beside the tests that need them.
pub struct Harness {
    /// Held for Drop: owns the fixture repo and git cache on disk.
    pub _fixture: tempfile::TempDir,
    pub repo_dir: std::path::PathBuf,
    pub cache: std::path::PathBuf,
    pub fixture_url: String,
    pub db_name: String,
    pub base: String,
    pub _server: yg_api::RunningServer,
    pub sync: yg_sync::SyncWorker,
    pub indexer: yg_index::IndexWorker,
    pub store: std::sync::Arc<dyn object_store::ObjectStore>,
}

impl Harness {
    pub async fn boot() -> Self {
        Self::boot_around(go_fixture_repo()).await
    }

    /// Boot around a custom fixture for tests that need a particular
    /// graph shape.
    pub async fn boot_with(files: &[(&str, &str)]) -> Self {
        Self::boot_around(fixture_repo_with(files)).await
    }

    pub async fn boot_around(
        (fixture, repo_dir, fixture_url): (tempfile::TempDir, std::path::PathBuf, String),
    ) -> Self {
        let cache = fixture.path().join("git-cache");
        let db_name = create_test_db().await;
        let mut config = test_config(&db_name);
        // Under the fixture guard so Drop reclaims it — shard caches
        // left in the system temp dir would pile up run after run.
        config.shard_cache = fixture.path().join("shard-cache");
        let store = config
            .object_store
            .connect()
            .expect("dev MinIO must be reachable");
        let server = serve(config).await.expect("boot");
        let base = format!("http://{}", server.local_addr());
        let sync = yg_sync::SyncWorker::new(control_plane(&db_name).await, &cache);
        let indexer =
            yg_index::IndexWorker::new(control_plane(&db_name).await, store.clone(), &cache);
        Self {
            _fixture: fixture,
            repo_dir,
            cache,
            fixture_url,
            db_name,
            base,
            _server: server,
            sync,
            indexer,
            store,
        }
    }

    pub async fn add_repo(&self) {
        post_repo(&self.base, serde_json::json!({"url": self.fixture_url})).await;
    }

    /// Drive one fetch + one index through the workers.
    pub async fn sync_and_index(&self) {
        assert!(self.sync.run_once().await.unwrap(), "fetch job must run");
        assert!(
            self.indexer
                .run_once()
                .await
                .expect("indexing must not error"),
            "a successful fetch must queue an index job"
        );
    }

    /// The repo qualifier that prefixes this fixture's external node ids
    /// (RFC 0001 §5): the Forge root sans scheme joined with the slug —
    /// for a `file://` fixture, the repo's filesystem path.
    pub fn qualifier(&self) -> String {
        self.repo_dir.display().to_string()
    }

    /// POST a Verb request, returning (status, parsed body).
    pub async fn verb(&self, verb: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/verbs/{verb}", self.base))
            .bearer_auth(TEST_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        let body = serde_json::from_str(&text)
            .unwrap_or_else(|_| panic!("verb {verb} answered non-JSON ({status}): {text}"));
        (status, body)
    }

    /// POST a Verb request that must succeed, returning its body.
    pub async fn verb_ok(&self, verb: &str, body: serde_json::Value) -> serde_json::Value {
        let (status, body) = self.verb(verb, body).await;
        assert_eq!(status, 200, "verb must succeed, got: {body}");
        body
    }

    /// Run the yg binary as a Member against this harness's server.
    /// Off the runtime threads: the server answering the CLI lives on
    /// this test's runtime, so blocking on the child in place would
    /// deadlock the pair.
    pub async fn yg(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("yg"));
        cmd.env("YG_SERVER", &self.base)
            .env("YG_TOKEN", TEST_TOKEN)
            .args(args);
        tokio::task::spawn_blocking(move || cmd.output().expect("running yg"))
            .await
            .expect("yg task panicked")
    }

    /// Run yg expecting success, returning stdout.
    pub async fn yg_ok(&self, args: &[&str]) -> String {
        let out = self.yg(args).await;
        assert!(
            out.status.success(),
            "yg {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("yg output is UTF-8")
    }
}

/// A local git repository standing in for a Forge-hosted one, addressable
/// as `file://…/acme/widgets`. Returns (fixture root guard, repo dir, URL).
pub fn fixture_repo(commits: usize) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let (root, repo, url) = empty_fixture_repo();
    for n in 1..=commits {
        std::fs::write(repo.join("README.md"), format!("revision {n}\n")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", &format!("commit {n}")]);
    }
    (root, repo, url)
}

/// Like [`fixture_repo`], but holding one commit of the given files.
/// Identical fixtures across tests mint identical commit shas, and
/// that's fine: every test database keys the shared dev bucket under
/// its own [`test_config`] prefix, so Shards can never collide.
pub fn fixture_repo_with(
    files: &[(&str, &str)],
) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let (root, repo, url) = empty_fixture_repo();
    for (path, contents) in files {
        let full = repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);
    (root, repo, url)
}

/// The shared scaffold: an initialized, empty repo with the machine-config
/// guards every fixture needs.
fn empty_fixture_repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
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
    let url = format!("file://{}", repo.display());
    (root, repo, url)
}
