//! End-to-end behavior tests, run against the dev compose stack — bring
//! it up with the sequence in docs/DEVELOPMENT.md "Checks" (CI runs the
//! same sequence).

mod common;

use common::*;
use yg_api::serve;

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
async fn discovery_keeps_private_repos_discovered_until_a_private_include_rule_matches() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;

    let forge = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "github",
            base_url: "https://github.com",
            org_slug: "acme",
            token_env: Some("YG_GITHUB_TOKEN"),
            api_root: None,
        })
        .await
        .unwrap();

    control
        .discover_forge_repos(
            forge.org_id,
            &[yg_control::DiscoveredRepo {
                slug: "acme/private-widgets",
                visibility: yg_control::RepoVisibility::Private,
                fetch_depth: None,
            }],
        )
        .await
        .unwrap();

    let repos = control.admin_status().await.unwrap();
    assert_eq!(repos[0].visibility, yg_control::RepoVisibility::Private);
    assert_eq!(repos[0].discovery_state, "discovered");
    assert!(
        repos[0].job_state.is_none(),
        "a private repo must not queue fetch before opt-in"
    );

    control
        .add_rule(yg_control::AddRule {
            forge_id: forge.forge_id,
            pattern: "acme/private-*",
            action: yg_control::RuleAction::Include,
            applies_to_private: true,
        })
        .await
        .unwrap();

    let repos = control.admin_status().await.unwrap();
    assert_eq!(repos[0].discovery_state, "included");
    assert_eq!(
        repos[0].job_state.as_deref(),
        Some("queued"),
        "explicit private opt-in must queue the first fetch"
    );
}

#[tokio::test]
async fn admin_repo_add_opts_in_a_previously_discovered_private_repo() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;

    let forge = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "github",
            base_url: "https://github.com",
            org_slug: "acme",
            token_env: None,
            api_root: None,
        })
        .await
        .unwrap();
    control
        .discover_forge_repos(
            forge.org_id,
            &[yg_control::DiscoveredRepo {
                slug: "acme/private-widgets",
                visibility: yg_control::RepoVisibility::Private,
                fetch_depth: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        control.admin_status().await.unwrap()[0].discovery_state,
        "discovered",
        "private org discovery must not include the repo by itself"
    );

    let added = control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: None,
            api_root: None,
            slug: "acme/private-widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();

    assert!(
        added.fetch_queued,
        "manual add is an explicit private opt-in"
    );
    let repos = control.admin_status().await.unwrap();
    assert_eq!(repos[0].visibility, yg_control::RepoVisibility::Private);
    assert_eq!(repos[0].discovery_state, "included");
    assert!(
        control
            .claim_due_fetch(std::time::Duration::from_secs(60))
            .await
            .unwrap()
            .is_some(),
        "the explicit opt-in fetch must be claimable"
    );
}

#[tokio::test]
async fn rediscovery_does_not_requeue_repos_that_are_already_included() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let forge = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "github",
            base_url: "https://github.com",
            org_slug: "acme",
            token_env: None,
            api_root: None,
        })
        .await
        .unwrap();
    let repos = [yg_control::DiscoveredRepo {
        slug: "acme/widgets",
        visibility: yg_control::RepoVisibility::Public,
        fetch_depth: None,
    }];

    assert_eq!(
        control
            .discover_forge_repos(forge.org_id, &repos)
            .await
            .unwrap(),
        1,
        "first discovery queues the initial fetch"
    );
    let fetch = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("initial discovery queued a fetch");
    assert!(
        control
            .complete_fetch(&fetch, "feedface0000000000000000000000000000feed")
            .await
            .unwrap(),
        "initial fetch completion lands"
    );

    assert_eq!(
        control
            .discover_forge_repos(forge.org_id, &repos)
            .await
            .unwrap(),
        0,
        "rediscovery of an already-included repo must not queue another fetch"
    );
    assert!(
        control
            .claim_due_fetch(std::time::Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "polling, not discovery, owns subsequent syncs for existing repos"
    );
}

#[tokio::test]
async fn exclude_rules_cancel_pending_fetches_for_included_repos() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: None,
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();
    let forge_id = control
        .forge_id_by_base_url("https://github.com")
        .await
        .unwrap()
        .unwrap();

    control
        .add_rule(yg_control::AddRule {
            forge_id,
            pattern: "acme/widgets",
            action: yg_control::RuleAction::Exclude,
            applies_to_private: false,
        })
        .await
        .unwrap();

    assert!(
        control
            .claim_due_fetch(std::time::Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "an excluded repo must not keep its queued fetch"
    );
    let repos = control.admin_status().await.unwrap();
    assert_eq!(repos[0].discovery_state, "excluded");
}

#[tokio::test]
async fn readding_a_repo_makes_its_exact_include_rule_newest_again() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: None,
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();
    let forge_id = control
        .forge_id_by_base_url("https://github.com")
        .await
        .unwrap()
        .unwrap();
    control
        .add_rule(yg_control::AddRule {
            forge_id,
            pattern: "acme/widgets",
            action: yg_control::RuleAction::Exclude,
            applies_to_private: false,
        })
        .await
        .unwrap();
    assert_eq!(
        control.admin_status().await.unwrap()[0].discovery_state,
        "excluded"
    );

    let added = control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: None,
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();

    assert!(added.fetch_queued);
    assert_eq!(
        control.admin_status().await.unwrap()[0].discovery_state,
        "included",
        "the latest equal-length rule must win the deterministic tie-break"
    );
}

#[tokio::test]
async fn exclude_rules_remove_indexed_repos_from_query_visibility() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let before = h
        .verb_ok("search", serde_json::json!({"query": "Hello"}))
        .await;
    assert!(
        before["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty()),
        "the fixture is queryable before exclusion: {before}"
    );

    let locator = yg_sync::RepoLocator::parse(&h.fixture_url).unwrap();
    let control = control_plane(&h.db_name).await;
    let forge_id = control
        .forge_id_by_base_url(&locator.base_url)
        .await
        .unwrap()
        .unwrap();
    control
        .add_rule(yg_control::AddRule {
            forge_id,
            pattern: &locator.slug,
            action: yg_control::RuleAction::Exclude,
            applies_to_private: false,
        })
        .await
        .unwrap();

    let after = h
        .verb_ok("search", serde_json::json!({"query": "Hello"}))
        .await;
    assert_eq!(
        after["hits"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new(),
        "excluded repos must leave org-wide search: {after}"
    );
    let status = admin_status_body(&h.base).await;
    assert_eq!(status["repos"][0]["discovery_state"], "excluded");
    assert_eq!(
        status["repos"][0]["shard"],
        serde_json::Value::Null,
        "excluding a repo must clear its current Shard pointer"
    );
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", h.db_name))
        .await
        .unwrap();
    let (superseded,): (bool,) = sqlx::query_as("SELECT superseded_at IS NOT NULL FROM shards")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(superseded, "the removed Shard keeps a GC grace anchor");
}

#[tokio::test]
async fn admin_forge_add_connects_a_github_org() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/admin/forges"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "kind": "github",
            "org": "acme",
            "token_env": "YG_GITHUB_TOKEN",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["kind"], "github");
    assert_eq!(body["org"], "acme");
    assert_eq!(body["base_url"], "https://github.com");
}

#[tokio::test]
async fn admin_forge_add_normalizes_base_url_and_defaults_the_github_token_env() {
    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.unwrap();
    let base = format!("http://{}", server.local_addr());

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/admin/forges"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "kind": "GitHub",
            "org": "acme",
            "base_url": "HTTPS://GitHub.COM/",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["kind"], "github");
    assert_eq!(body["base_url"], "https://github.com");

    let control = control_plane(&db_name).await;
    let due = control
        .claim_due_discovery(std::time::Duration::from_secs(3600))
        .await
        .unwrap()
        .expect("connected forge org must be due for discovery");
    assert_eq!(due.base_url, "https://github.com");
    assert_eq!(due.token_env.as_deref(), Some("YG_GITHUB_TOKEN"));
    assert_eq!(
        due.api_root.as_deref(),
        Some("https://api.github.com"),
        "registration must record the forge's API root on the record"
    );
}

/// A repo add on a GitHub Enterprise forge arrives as generic `git` —
/// the URL locator can't recognize arbitrary enterprise hosts — and
/// must not reclassify the forge row that `forge add` registered as
/// `github`, or the org's discovery would silently stop resolving an
/// adapter.
#[tokio::test]
async fn adding_a_repo_does_not_reclassify_an_enterprise_forge() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "github",
            base_url: "https://github.enterprise.example",
            org_slug: "acme",
            token_env: Some("YG_GITHUB_TOKEN"),
            api_root: Some("https://github.enterprise.example/api/v3"),
        })
        .await
        .unwrap();

    // What `admin repo add` sends for an enterprise URL: the locator
    // falls back to the generic git kind for an unrecognized host.
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "git",
            base_url: "https://github.enterprise.example",
            token_env: None,
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();

    let due = control
        .claim_due_discovery(std::time::Duration::from_secs(3600))
        .await
        .unwrap()
        .expect("the org must still be due for discovery");
    assert_eq!(
        due.forge_kind, "github",
        "a repo add must not flip the forge kind and disable discovery"
    );
    assert_eq!(
        due.api_root.as_deref(),
        Some("https://github.enterprise.example/api/v3"),
        "the registered API root must survive the repo add"
    );
}

/// Adding a forge is one trait implementation plus registration (#53):
/// a double registered under its own kind flows through the same
/// discovery loop as GitHub — claim, adapter listing, reconciliation —
/// with no other code changes.
#[tokio::test]
async fn a_registered_forge_double_discovers_repos_through_the_worker_loop() {
    use std::sync::Arc;
    use yg_sync::forge::{BoxFuture, Forge, ForgeRegistry, GitAuth, ListedRepo, OrgDiscovery};

    struct FakeForge;

    impl Forge for FakeForge {
        fn kind(&self) -> &'static str {
            "fakeforge"
        }
        fn claims_host(&self, host: &str) -> bool {
            host == "fake.example"
        }
        fn default_token_env(&self) -> Option<&'static str> {
            None
        }
        fn default_api_root(&self, base_url: &str) -> Option<String> {
            Some(format!("{base_url}/api"))
        }
        fn git_auth(&self, token: String) -> GitAuth {
            GitAuth {
                username: "fake-user",
                token,
            }
        }
        fn is_rate_limit(&self, _message: &str) -> bool {
            false
        }
        fn discovery(&self) -> Option<&dyn OrgDiscovery> {
            Some(self)
        }
    }

    impl OrgDiscovery for FakeForge {
        fn list_org_repos<'a>(
            &'a self,
            _client: &'a reqwest::Client,
            api_root: &'a str,
            org: &'a str,
            _token: Option<&'a str>,
        ) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>> {
            // The API root must be the record's explicit field, not an
            // inference from the clone root.
            assert_eq!(api_root, "https://fake.example/api");
            let org = org.to_string();
            Box::pin(async move {
                Ok(vec![ListedRepo {
                    slug: format!("{org}/discovered-widgets"),
                    visibility: yg_control::RepoVisibility::Public,
                }])
            })
        }
    }

    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "fakeforge",
            base_url: "https://fake.example",
            org_slug: "acme",
            token_env: None,
            api_root: Some("https://fake.example/api"),
        })
        .await
        .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let worker = yg_sync::SyncWorker::with_registry(
        control_plane(&db_name).await,
        cache.path(),
        ForgeRegistry::builtin().register(Arc::new(FakeForge)),
    );
    let had_claim = worker
        .discover_once(&yg_sync::DiscoveryConfig {
            interval: std::time::Duration::from_secs(3600),
        })
        .await
        .unwrap();
    assert!(had_claim, "the connected org must be due for discovery");

    let repos = control.admin_status().await.unwrap();
    assert_eq!(repos.len(), 1, "the double's listing must be reconciled");
    assert_eq!(repos[0].slug, "acme/discovered-widgets");
    assert_eq!(repos[0].discovery_state, "included");
}

#[tokio::test]
async fn reconnecting_a_forge_org_refreshes_the_token_env() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "github",
            base_url: "https://github.com",
            org_slug: "acme",
            token_env: Some("YG_OLD_TOKEN"),
            api_root: None,
        })
        .await
        .unwrap();
    control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "github",
            base_url: "https://github.com",
            org_slug: "acme",
            token_env: Some("YG_NEW_TOKEN"),
            api_root: None,
        })
        .await
        .unwrap();

    let due = control
        .claim_due_discovery(std::time::Duration::from_secs(3600))
        .await
        .unwrap()
        .expect("connected forge org must be due for discovery");
    assert_eq!(due.token_env.as_deref(), Some("YG_NEW_TOKEN"));
}

#[tokio::test]
async fn admin_forge_add_rejects_malformed_orgs_and_base_urls() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    for body in [
        serde_json::json!({"kind": "github", "org": "bad/org"}),
        serde_json::json!({"kind": "github", "org": "-bad"}),
        serde_json::json!({"kind": "github", "org": "bad--org"}),
        serde_json::json!({"kind": "github", "org": "acme", "base_url": "http://github.com"}),
        serde_json::json!({"kind": "github", "org": "acme", "base_url": "https://token@github.com"}),
        serde_json::json!({"kind": "github", "org": "acme", "base_url": "https://github.com/acme"}),
    ] {
        let resp = client
            .post(format!("{base}/v1/admin/forges"))
            .bearer_auth(TEST_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "body must be rejected: {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn yg_admin_forge_add_and_rules_manage_discovery_policy() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    let forge = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &base)
        .env("YG_TOKEN", TEST_TOKEN)
        .timeout(std::time::Duration::from_secs(10))
        .args([
            "admin",
            "forge",
            "add",
            "github",
            "acme",
            "--token-env",
            "YG_GITHUB_TOKEN",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(forge.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("connected github org acme"),
        "forge add output must name the connection, got:\n{stdout}"
    );

    let discover = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &base)
        .env("YG_TOKEN", TEST_TOKEN)
        .timeout(std::time::Duration::from_secs(10))
        .args(["admin", "forge", "discover", "github", "acme"])
        .assert()
        .success();
    let stdout = String::from_utf8(discover.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("discovery requested for github org acme"),
        "forge discover output must name the on-demand request, got:\n{stdout}"
    );

    let rule = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &base)
        .env("YG_TOKEN", TEST_TOKEN)
        .timeout(std::time::Duration::from_secs(10))
        .args([
            "admin",
            "rules",
            "add",
            "acme/private-*",
            "--action",
            "include",
            "--forge",
            "https://GITHUB.COM/",
            "--private",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(rule.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("include acme/private-*"),
        "rule add output must name the deterministic policy, got:\n{stdout}"
    );

    let rules = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &base)
        .env("YG_TOKEN", TEST_TOKEN)
        .timeout(std::time::Duration::from_secs(10))
        .args(["admin", "rules", "list"])
        .assert()
        .success();
    let stdout = String::from_utf8(rules.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("https://github.com  include  acme/private-*  private"),
        "rules list must expose the private opt-in rule, got:\n{stdout}"
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
    let mirror = only_mirror(&cache);
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
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
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
    let mirror_name = only_mirror(&cache).file_name().unwrap().to_owned();
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
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
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
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
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
async fn a_heartbeating_worker_keeps_a_job_whose_work_outlives_the_base_lease() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: Some("YG_GITHUB_TOKEN"),
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();

    // The base lease (zero — already expired) stands in for a fetch that
    // outlives it: without a heartbeat, the job would be claimable again.
    let job = control
        .claim_due_fetch(std::time::Duration::ZERO)
        .await
        .unwrap()
        .expect("claimable");

    // A heartbeat renews the lease before anyone reclaims the job…
    assert!(
        control
            .renew_fetch(&job, std::time::Duration::from_secs(60))
            .await
            .unwrap(),
        "the lease holder's renewal must apply"
    );

    // …so an idle worker can no longer steal it…
    assert!(
        control
            .claim_due_fetch(std::time::Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "a renewed lease must block other workers"
    );

    // …and the (renewed) worker's completion lands normally.
    assert!(
        control
            .complete_fetch(&job, "feedface0000000000000000000000000000feed")
            .await
            .unwrap(),
        "completion under a renewed lease must apply"
    );
    let repos = control.admin_status().await.unwrap();
    assert_eq!(
        repos[0].last_synced_commit.as_deref(),
        Some("feedface0000000000000000000000000000feed")
    );
}

#[tokio::test]
async fn a_reclaimed_jobs_old_lease_token_can_neither_renew_nor_settle() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: Some("YG_GITHUB_TOKEN"),
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();

    // Worker A heartbeats twice — each renewal minting a fresh token, the
    // zero lease keeping the deadline in the past — then stops; worker B
    // reclaims the expired job. The fenced token below is thus a renewed
    // token, not the claim's original.
    let stale = control
        .claim_due_fetch(std::time::Duration::ZERO)
        .await
        .unwrap()
        .expect("claimable");
    assert!(
        control
            .renew_fetch(&stale, std::time::Duration::ZERO)
            .await
            .unwrap(),
        "a renewal chains off the claim's token"
    );
    assert!(
        control
            .renew_fetch(&stale, std::time::Duration::ZERO)
            .await
            .unwrap(),
        "a renewal chains off the previous renewal's token"
    );
    let fresh = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("an expired lease must make the job claimable again");

    // A's old token is fenced out of every operation: renew, complete, fail.
    assert!(
        !control
            .renew_fetch(&stale, std::time::Duration::from_secs(60))
            .await
            .unwrap(),
        "a reclaimed job's old token must not renew"
    );
    assert!(
        !control
            .complete_fetch(&stale, "deadbeef0000000000000000000000000000dead")
            .await
            .unwrap(),
        "a reclaimed job's old token must not complete"
    );
    assert!(
        !control.fail_fetch(&stale, "boom").await.unwrap(),
        "a reclaimed job's old token must not fail the job"
    );
    let repos = control.admin_status().await.unwrap();
    assert_eq!(repos[0].last_synced_commit, None);
    assert_eq!(repos[0].attempts, 0, "a fenced failure must not count");
    assert_eq!(
        repos[0].job_state.as_deref(),
        Some("leased"),
        "the job must still belong to worker B"
    );

    // B's renewals and completion still land.
    assert!(
        control
            .renew_fetch(&fresh, std::time::Duration::from_secs(60))
            .await
            .unwrap(),
        "the live lease holder's renewal must apply"
    );
    assert!(
        control
            .complete_fetch(&fresh, "feedface0000000000000000000000000000feed")
            .await
            .unwrap(),
        "the live lease holder's completion must apply after renewing"
    );
}

#[tokio::test]
async fn index_lease_renewal_is_fenced_the_same_way() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "github",
            base_url: "https://github.com",
            token_env: Some("YG_GITHUB_TOKEN"),
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();

    // A completed fetch queues the index job and sets the sync position
    // an index claim requires.
    let fetch = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("claimable");
    assert!(
        control
            .complete_fetch(&fetch, "feedface0000000000000000000000000000feed")
            .await
            .unwrap()
    );

    // Worker A's index lease lapses mid-parse; worker B reclaims the job.
    let stale = control
        .claim_due_index(std::time::Duration::ZERO)
        .await
        .unwrap()
        .expect("the queued index job must be claimable");
    let fresh = control
        .claim_due_index(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("an expired index lease must be claimable again");
    assert_eq!(fresh.job_id, stale.job_id, "same job, not a copy");

    // A's old token can neither renew nor settle the job…
    assert!(
        !control
            .renew_index(&stale, std::time::Duration::from_secs(60))
            .await
            .unwrap(),
        "a reclaimed index job's old token must not renew"
    );
    assert!(
        !control
            .complete_index(
                &stale,
                yg_control::ShardRecord {
                    revision: "stale-rev-syntactic-v0",
                    manifest_key: "stale/manifest",
                    commit_sha: "feedface0000000000000000000000000000feed",
                    provenance_level: "syntactic",
                    node_count: 0,
                    edge_count: 0,
                },
            )
            .await
            .unwrap(),
        "a reclaimed index job's old token must not complete"
    );
    assert!(
        !control.fail_index(&stale, "boom").await.unwrap(),
        "a reclaimed index job's old token must not settle"
    );

    // …while B's heartbeat keeps the job through work that outlives the
    // base lease, and its settlement lands.
    assert!(
        control
            .renew_index(&fresh, std::time::Duration::from_secs(60))
            .await
            .unwrap(),
        "the live lease holder's renewal must apply"
    );
    assert!(
        control
            .claim_due_index(std::time::Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "a renewed index lease must block other workers"
    );
    assert!(
        control.fail_index(&fresh, "parse blew up").await.unwrap(),
        "the live lease holder's settlement must apply after renewing"
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
        .env("YG_TOKEN", TEST_TOKEN)
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
async fn admin_repo_add_rejects_non_positive_poll_interval() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    for interval in [0, -3] {
        let resp = post_repo(
            &base,
            serde_json::json!({
                "url": "https://github.com/acme/widgets",
                "poll_interval": interval
            }),
        )
        .await;
        assert_eq!(
            resp.status(),
            400,
            "poll_interval {interval} must be rejected"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.contains("poll_interval")),
            "the error must name poll_interval, got: {body}"
        );
    }

    let body = admin_status_body(&base).await;
    assert_eq!(
        body["repos"].as_array().unwrap().len(),
        0,
        "a rejected poll_interval must not register the repo, got: {body}"
    );

    // The CLI rejects it before the request even leaves.
    let cli = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &base)
        .env("YG_TOKEN", TEST_TOKEN)
        .args([
            "admin",
            "repo",
            "add",
            "https://github.com/acme/widgets",
            "--poll-interval",
            "0",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8(cli.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("poll-interval") || stderr.contains("--poll-interval"),
        "clap must reject poll interval 0, got:\n{stderr}"
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

    let commit_count_in_cache =
        |cache: std::path::PathBuf| git(&only_mirror(&cache), &["rev-list", "--count", "HEAD"]);

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

    let mirror = only_mirror(&cache);
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
        ("YG_TOKEN", TEST_TOKEN.into()),
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

/// multi_thread: the yg binary blocks this thread while the in-process
/// server it queries runs on the same test runtime.
#[tokio::test(flavor = "multi_thread")]
async fn admin_status_summarizes_mixed_repository_visibility() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let forge = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "github",
            base_url: "https://github.com",
            org_slug: "acme",
            token_env: None,
            api_root: None,
        })
        .await
        .unwrap();
    control
        .discover_forge_repos(
            forge.org_id,
            &[
                yg_control::DiscoveredRepo {
                    slug: "acme/public-widgets",
                    visibility: yg_control::RepoVisibility::Public,
                    fetch_depth: None,
                },
                yg_control::DiscoveredRepo {
                    slug: "acme/internal-widgets",
                    visibility: yg_control::RepoVisibility::Internal,
                    fetch_depth: None,
                },
                yg_control::DiscoveredRepo {
                    slug: "acme/private-widgets",
                    visibility: yg_control::RepoVisibility::Private,
                    fetch_depth: None,
                },
            ],
        )
        .await
        .unwrap();

    let server = serve(test_config(&db_name)).await.unwrap();
    let env = [
        ("YG_SERVER", format!("http://{}", server.local_addr())),
        ("YG_TOKEN", TEST_TOKEN.into()),
    ];

    let status = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .envs(env.iter().cloned())
        .args(["admin", "status"])
        .assert()
        .success();
    let stdout = String::from_utf8(status.get_output().stdout.clone()).unwrap();
    assert_eq!(
        stdout.lines().find(|line| line.starts_with("visibility:")),
        Some("visibility: public=1 internal=1 private=1"),
        "human status must summarize visibility counts, got:\n{stdout}"
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
    assert_eq!(
        body["visibility_counts"],
        serde_json::json!({
            "public": 1,
            "internal": 1,
            "private": 1,
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn yg_serve_role_all_syncs_an_added_repo_without_a_separate_worker() {
    let (fixture, repo_dir, fixture_url) = fixture_repo(1);
    let head = git(&repo_dir, &["rev-parse", "HEAD"]);

    let db_name = create_test_db().await;
    let (_server, url) = spawn_yg_serve(&db_name, |cmd| {
        cmd.env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN)
            .env("YG_GIT_CACHE", fixture.path().join("git-cache"));
    });

    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &url)
        .env("YG_TOKEN", TEST_TOKEN)
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
    let control = control_plane(&db_name).await;
    let locator = fixture_url.strip_prefix("file://").unwrap();
    let (base, slug) = locator.rsplit_once("/acme/").unwrap();
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "git",
            base_url: &format!("file://{base}"),
            token_env: None,
            api_root: None,
            slug: &format!("acme/{slug}"),
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();

    // Worker role: no HTTP, no bootstrap token — just the queue.
    let _worker = spawn_yg_worker(&db_name, |cmd| {
        cmd.env_remove("YG_BOOTSTRAP_TOKEN")
            .env("YG_GIT_CACHE", fixture.path().join("git-cache"));
    });

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

/// A real API process must honor the deploy-configured cache ceiling, not
/// merely the in-process constructor seam. The fixture derives its cap from
/// the artifacts it just published, so the test never guesses at SQLite file
/// sizes: one graph fits, while every pair is known to exceed the cap.
#[tokio::test(flavor = "multi_thread")]
async fn spawned_api_evicts_lru_shards_under_the_configured_byte_cap() {
    use object_store::ObjectStoreExt;

    let (fixture, repo_dir, fixture_url) = go_fixture_repo();
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let locator = fixture_url.strip_prefix("file://").unwrap();
    let (base, slug) = locator.rsplit_once("/acme/").unwrap();
    let added = control
        .add_repo(yg_control::AddRepo {
            forge_kind: "git",
            base_url: &format!("file://{base}"),
            token_env: None,
            api_root: None,
            slug: &format!("acme/{slug}"),
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();

    let git_cache = fixture.path().join("git-cache");
    let sync = yg_sync::SyncWorker::new(control_plane(&db_name).await, &git_cache);
    let store = dev_object_store(&db_name);
    let indexer =
        yg_index::IndexWorker::new(control_plane(&db_name).await, store.clone(), &git_cache);
    let mut artifacts = Vec::new();

    for generation in 1..=3 {
        if generation > 1 {
            std::fs::write(
                repo_dir.join("README.md"),
                format!("# gadgets\n\nrevision {generation}\n"),
            )
            .unwrap();
            git(&repo_dir, &["add", "."]);
            git(
                &repo_dir,
                &["commit", "-m", &format!("revision {generation}")],
            );
            assert!(
                control.request_fetch(added.repo_id).await.unwrap(),
                "the next revision must queue a fresh fetch"
            );
        }

        assert!(
            sync.run_once().await.unwrap(),
            "fetch generation {generation}"
        );
        assert!(
            indexer.run_once().await.unwrap(),
            "index generation {generation}"
        );
        let revision = control.admin_status().await.unwrap()[0]
            .shard_revision
            .clone()
            .expect("indexed repo has a Shard revision");
        let manifest = store
            .get(&object_store::path::Path::from(yg_shard::manifest_key(
                added.repo_id,
                &revision,
            )))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let manifest: yg_shard::Manifest = serde_json::from_slice(&manifest).unwrap();
        let graph = &manifest.segments[yg_shard::GRAPH_SEGMENT_FILE];
        artifacts.push((revision, graph.sha256.clone(), graph.bytes));
    }

    let capacity = artifacts.iter().map(|(_, _, bytes)| *bytes).max().unwrap();
    for left in 0..artifacts.len() {
        for right in left + 1..artifacts.len() {
            assert!(
                artifacts[left].2 + artifacts[right].2 > capacity,
                "the derived cap must fit one graph but not two: {artifacts:?}"
            );
        }
    }
    let unique_shas: std::collections::HashSet<&str> =
        artifacts.iter().map(|(_, sha, _)| sha.as_str()).collect();
    assert_eq!(
        unique_shas.len(),
        artifacts.len(),
        "each revision must publish distinct graph content"
    );

    let shard_cache = fixture.path().join("shard-cache");
    let (_api, url) = spawn_yg_api(&db_name, |cmd| {
        cmd.env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN)
            .env("YG_SHARD_CACHE", &shard_cache)
            .env("YG_SHARD_CACHE_MAX_BYTES", capacity.to_string());
    });
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    let node_id = format!("sym:{}:main.go#Hello", repo_dir.display());

    for (revision, _, _) in &artifacts {
        sqlx::query(
            "UPDATE repos
             SET current_shard_id = (
                 SELECT id FROM shards WHERE repo_id = $1 AND revision = $2
             )
             WHERE id = $1",
        )
        .bind(added.repo_id)
        .bind(revision)
        .execute(&pool)
        .await
        .unwrap();

        let response = reqwest::Client::new()
            .post(format!("{url}/v1/verbs/node"))
            .bearer_auth(TEST_TOKEN)
            .json(&serde_json::json!({"id": node_id}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(
            status.is_success(),
            "node at {revision} returned {status}: {body}"
        );

        let disk_bytes: u64 = std::fs::read_dir(&shard_cache)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(
            disk_bytes <= capacity,
            "cache uses {disk_bytes} bytes over its {capacity}-byte cap"
        );
    }

    let first = shard_cache.join(format!("{}.sqlite", artifacts[0].1));
    let last = shard_cache.join(format!("{}.sqlite", artifacts[2].1));
    assert!(!first.exists(), "the least-recent graph must be evicted");
    assert!(last.is_file(), "the newest graph must remain cached");
}

/// The documented split topology (docs/DEVELOPMENT.md): one process
/// serves HTTP, a separate process drains the queues, and they meet
/// only in Postgres and object storage. The repo-add flows API →
/// queue, the worker syncs and indexes it, and the API process reads
/// the published Shard back through the search Verb.
#[tokio::test(flavor = "multi_thread")]
async fn split_api_and_worker_processes_index_and_serve_a_repo() {
    let (fixture, _repo_dir, fixture_url) = go_fixture_repo();
    let db_name = create_test_db().await;

    let (_api, url) = spawn_yg_api(&db_name, |cmd| {
        cmd.env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN);
    });
    let _worker = spawn_yg_worker(&db_name, |cmd| {
        cmd.env_remove("YG_BOOTSTRAP_TOKEN")
            .env("YG_GIT_CACHE", fixture.path().join("git-cache"));
    });

    post_repo(&url, serde_json::json!({"url": fixture_url})).await;
    await_symbol(&url, "Hello", std::time::Duration::from_secs(60)).await;

    // The published Shard lives under this database's key namespace —
    // observed through an *un-prefixed* client, so the isolation is in
    // the bucket, not a client-side illusion.
    let raw = dev_object_store("");
    let under_prefix = raw
        .list_with_delimiter(Some(&object_store::path::Path::from(db_name.as_str())))
        .await
        .unwrap();
    assert!(
        !under_prefix.common_prefixes.is_empty() || !under_prefix.objects.is_empty(),
        "the Shard must be keyed under the test database's prefix {db_name}"
    );
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
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(authed_unknown.status(), 404, "valid token sees real 404s");

    // RFC 9110: the auth scheme is case-insensitive.
    let lowercase_scheme = client
        .get(format!("{base}/v1/status"))
        .header("Authorization", format!("bearer {TEST_TOKEN}"))
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
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let uptime = resp
        .headers()
        .get(yg_api::UPTIME_HEADER)
        .expect("uptime is volatile, so it rides in a header")
        .to_str()
        .unwrap()
        .to_string();
    uptime
        .parse::<u64>()
        .expect("uptime header must be seconds");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["repos_indexed"], 0, "fresh deployment indexes nothing");
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
        .bearer_auth(TEST_TOKEN)
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
        .env("YG_TOKEN", TEST_TOKEN)
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
        .env("YG_TOKEN", TEST_TOKEN)
        .args(["status", "--json"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json output must be valid JSON");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["repos_indexed"], 0);
    assert!(
        body["uptime_seconds"].is_u64(),
        "the CLI folds the uptime header back in for machine consumers, got: {body}"
    );
}

/// An internal failure (the control plane severed under a live server)
/// answers with a generic 500 body — no database errors, hosts, or paths —
/// while the full error chain lands in the server's logs.
#[tokio::test]
async fn internal_failures_return_a_sanitized_500_and_log_the_full_chain() {
    // Capture this process's tracing output: the in-process server logs
    // the chain through the global subscriber.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Sink::default();
    let writer = sink.clone();
    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .try_init()
        .expect("test requires setting the global tracing subscriber");

    let db_name = create_test_db().await;
    let server = serve(test_config(&db_name)).await.expect("boot");

    // Sever the control plane out from under the running server, then
    // drive a request whose handler must reach it.
    let admin = admin_pool().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"DROP DATABASE "{db_name}" WITH (FORCE)"#
    )))
    .execute(&admin)
    .await
    .unwrap();

    let resp = reqwest::Client::new()
        .get(format!("http://{}/v1/status", server.local_addr()))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500, "a severed dependency is a server fault");
    let text = resp.text().await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body["error"], "internal server error",
        "500 bodies carry no internal detail, got: {text}"
    );
    assert!(
        !text.contains(&db_name) && !text.to_lowercase().contains("database"),
        "500 bodies must not leak the error chain: {text}"
    );

    // The chain must appear on the sanitizer's own log line — the db name
    // showing up in some other writer's output (e.g. sqlx statement
    // logging) must not satisfy this.
    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.lines()
            .any(|line| line.contains("internal error") && line.contains(&db_name)),
        "the full error chain must appear on the internal-error log line, got:\n{logs}"
    );
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
    let text = resp.text().await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["status"], "degraded");
    assert_eq!(
        body["checks"]["object_store"], "ok",
        "storage is still fine"
    );
    // Anonymous callers get a bare verdict — never the failure detail
    // (hosts, ports, database names ride in connection errors).
    assert_eq!(
        body["checks"]["postgres"], "error",
        "the check is a bare ok/error verdict, got: {body}"
    );
    assert!(
        !text.contains(&db_name) && !text.contains("localhost") && !text.contains("5432"),
        "health must not leak dependency details to anonymous callers: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn yg_serve_boots_from_env_and_answers_yg_status_end_to_end() {
    let db_name = create_test_db().await;
    let (_server, url) = spawn_yg_serve(&db_name, |cmd| {
        // Padded on purpose: env files commonly leak whitespace
        // around secrets; clients present the clean token below.
        cmd.env("YG_BOOTSTRAP_TOKEN", " ygt_test_token\n");
    });

    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &url)
        .env("YG_TOKEN", TEST_TOKEN)
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
    assert_eq!(body["checks"]["postgres"], "ok");
    assert_eq!(body["checks"]["object_store"], "ok");
    // Health is anonymous: it reports readiness verdicts and nothing else
    // (no version, no dependency addresses).
    assert!(
        body.get("version").is_none(),
        "health must not advertise the server version, got: {body}"
    );
}
