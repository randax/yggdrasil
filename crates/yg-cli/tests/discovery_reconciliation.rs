mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{DEV_POSTGRES, admin_status_body, control_plane, create_test_db, test_config};
use yg_sync::forge::{BoxFuture, Forge, ForgeRegistry, GitAuth, ListedRepo, OrgDiscovery};

struct ListingForge {
    kind: &'static str,
    host: &'static str,
    slugs: &'static [&'static str],
}

impl Forge for ListingForge {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn claims_host(&self, host: &str) -> bool {
        host == self.host
    }

    fn default_token_env(&self) -> Option<&'static str> {
        None
    }

    fn default_api_root(&self, base_url: &str) -> Option<String> {
        Some(format!("{base_url}/api"))
    }

    fn git_auth(&self, token: String) -> GitAuth {
        GitAuth {
            username: "listing-forge",
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

impl OrgDiscovery for ListingForge {
    fn list_org_repos<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        _api_root: &'a str,
        _org: &'a str,
        _token: Option<&'a str>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<ListedRepo>>> {
        Box::pin(async move {
            Ok(self
                .slugs
                .iter()
                .map(|slug| ListedRepo {
                    slug: (*slug).to_owned(),
                    visibility: yg_control::RepoVisibility::Public,
                })
                .collect())
        })
    }
}

async fn worker_with_listing(
    db_name: &str,
    cache: &tempfile::TempDir,
    forge: ListingForge,
) -> yg_sync::SyncWorker {
    yg_sync::SyncWorker::with_registry(
        control_plane(db_name).await,
        cache.path(),
        ForgeRegistry::builtin().register(Arc::new(forge)),
    )
}

#[tokio::test]
async fn qualifier_conflict_is_recorded_while_neighboring_repos_reconcile() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "git",
            base_url: "https://collision.example",
            token_env: None,
            api_root: None,
            slug: "acme/conflict",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();
    let connected = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "collision-listing",
            base_url: "http://collision.example",
            org_slug: "acme",
            token_env: None,
            api_root: Some("http://collision.example/api"),
        })
        .await
        .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let worker = worker_with_listing(
        &db_name,
        &cache,
        ListingForge {
            kind: "collision-listing",
            host: "collision.example",
            slugs: &["acme/before", "acme/conflict", "acme/after"],
        },
    )
    .await;
    assert!(
        worker
            .discover_once(&yg_sync::DiscoveryConfig {
                interval: Duration::from_secs(3600),
            })
            .await
            .unwrap()
    );

    let statuses = control.admin_status().await.unwrap();
    for slug in ["acme/before", "acme/after"] {
        let status = statuses
            .iter()
            .find(|status| status.slug == slug)
            .unwrap_or_else(|| panic!("{slug} after the collision must reconcile"));
        assert_eq!(status.discovery_state, "included");
        assert_eq!(status.job_state.as_deref(), Some("queued"));
    }
    let conflicts = control_plane(&db_name)
        .await
        .admin_discovery_conflicts()
        .await
        .unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].slug.as_str(), "acme/conflict");
    assert_eq!(
        conflicts[0].qualifier.as_str(),
        "collision.example/acme/conflict"
    );

    let server = yg_api::serve(test_config(&db_name)).await.unwrap();
    let body = admin_status_body(&format!("http://{}", server.local_addr())).await;
    assert_eq!(
        body["discovery_conflicts"],
        serde_json::json!([{
            "forge": "http://collision.example",
            "org": "acme",
            "slug": "acme/conflict",
            "qualifier": "collision.example/acme/conflict"
        }])
    );

    control
        .discover_forge_repos(
            connected.org_id,
            &[yg_control::DiscoveredRepo {
                slug: "acme/after",
                visibility: yg_control::RepoVisibility::Public,
                fetch_depth: None,
            }],
        )
        .await
        .unwrap();
    assert!(
        control
            .admin_discovery_conflicts()
            .await
            .unwrap()
            .is_empty(),
        "a completed listing that omits the colliding slug resolves its status"
    );
}

#[tokio::test]
async fn same_slug_insert_racing_reconciliation_is_upserted_without_a_false_conflict() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let connected = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "git",
            base_url: "https://same-slug-race.example",
            org_slug: "acme",
            token_env: None,
            api_root: Some("https://same-slug-race.example/api"),
        })
        .await
        .unwrap();
    let stale_owner = control
        .add_repo(yg_control::AddRepo {
            forge_kind: "git",
            base_url: "https://same-slug-race.example",
            token_env: None,
            api_root: None,
            slug: "acme/stale-owner",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO forge_discovery_qualifier_conflicts
             (forge_org_id, slug, conflicting_repo_id)
         VALUES ($1, 'acme/raced', $2)",
    )
    .bind(connected.org_id)
    .bind(stale_owner.repo_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE FUNCTION insert_repo_at_reconciliation_seam() RETURNS trigger
         LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.slug = 'acme/raced' AND NEW.fetch_depth = 7 THEN
                 INSERT INTO repos
                     (forge_id, slug, visibility, discovery_state, qualifier,
                      registration_base_url)
                 VALUES
                     (NEW.forge_id, NEW.slug, 'public', 'included', NEW.qualifier,
                      NEW.registration_base_url);
             END IF;
             RETURN NEW;
         END
         $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER insert_repo_at_reconciliation_seam
         BEFORE INSERT ON repos
         FOR EACH ROW EXECUTE FUNCTION insert_repo_at_reconciliation_seam()",
    )
    .execute(&pool)
    .await
    .unwrap();

    control
        .discover_forge_repos(
            connected.org_id,
            &[yg_control::DiscoveredRepo {
                slug: "acme/raced",
                visibility: yg_control::RepoVisibility::Private,
                fetch_depth: Some(7),
            }],
        )
        .await
        .unwrap();

    let statuses = control.admin_status().await.unwrap();
    let raced = statuses
        .iter()
        .find(|status| status.slug == "acme/raced")
        .expect("the concurrently registered repo must be reconciled");
    assert_eq!(raced.visibility, yg_control::RepoVisibility::Private);
    assert_eq!(raced.discovery_state, "discovered");
    let (fetch_depth,): (Option<i32>,) = sqlx::query_as(
        "SELECT fetch_depth FROM repos
         WHERE qualifier = 'same-slug-race.example/acme/raced'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fetch_depth, Some(7));
    assert!(
        control
            .admin_discovery_conflicts()
            .await
            .unwrap()
            .is_empty(),
        "successful same-slug reconciliation must clear stale conflict state"
    );
}

#[derive(Clone, Default)]
struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn invalid_listed_slugs_are_logged_and_never_queued_for_fetch() {
    let sink = LogSink::default();
    let writer = sink.clone();
    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .try_init()
        .expect("this test target installs one tracing subscriber");

    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "invalid-listing",
            base_url: "https://invalid-listing.example",
            org_slug: "acme",
            token_env: None,
            api_root: Some("https://invalid-listing.example/api"),
        })
        .await
        .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let worker = worker_with_listing(
        &db_name,
        &cache,
        ListingForge {
            kind: "invalid-listing",
            host: "invalid-listing.example",
            slugs: &[
                "acme/valid",
                "acme/../escape",
                "acme/query?ref=main",
                "user:password@evil/repo",
                "acme/white space",
                "acme/non\u{a0}breaking",
            ],
        },
    )
    .await;
    worker
        .discover_once(&yg_sync::DiscoveryConfig {
            interval: Duration::from_secs(3600),
        })
        .await
        .unwrap();

    let statuses = control.admin_status().await.unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].slug, "acme/valid");
    let fetch = control
        .claim_due_fetch(Duration::from_secs(60))
        .await
        .unwrap()
        .expect("the valid listed repo must queue one fetch");
    assert_eq!(fetch.slug, "acme/valid");
    assert!(
        control
            .claim_due_fetch(Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "no invalid slug may become a fetch job"
    );

    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    for rejected in [
        "acme/../escape",
        "acme/query?ref=main",
        "user:password@evil/repo",
        "acme/white space",
        "acme/non\u{a0}breaking",
    ] {
        let logged = rejected.escape_debug().to_string();
        assert!(
            logs.contains(&logged),
            "the rejected slug {rejected:?} must be logged; got:\n{logs}"
        );
    }
    assert!(logs.contains("forge discovery rejected an invalid repository slug"));
}

#[tokio::test]
async fn overlapping_rule_changes_and_reverse_discovery_do_not_deadlock() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let forge = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "git",
            base_url: "https://locking.example",
            org_slug: "acme",
            token_env: None,
            api_root: Some("https://locking.example/api"),
        })
        .await
        .unwrap();
    let slugs: Vec<String> = (0..64)
        .map(|index| format!("acme/repo-{index:03}"))
        .collect();
    let initial: Vec<_> = slugs
        .iter()
        .map(|slug| yg_control::DiscoveredRepo {
            slug,
            visibility: yg_control::RepoVisibility::Public,
            fetch_depth: None,
        })
        .collect();
    control
        .discover_forge_repos(forge.org_id, &initial)
        .await
        .unwrap();

    for round in 0..8 {
        let reverse: Vec<_> = slugs
            .iter()
            .rev()
            .map(|slug| yg_control::DiscoveredRepo {
                slug,
                visibility: yg_control::RepoVisibility::Public,
                fetch_depth: Some(round + 1),
            })
            .collect();
        let discovery_control = control.clone();
        let rule_control = control.clone();
        let pair = async {
            tokio::join!(
                discovery_control.discover_forge_repos(forge.org_id, &reverse),
                rule_control.add_rule(yg_control::AddRule {
                    forge_id: forge.forge_id,
                    pattern: "acme/repo-*",
                    action: if round % 2 == 0 {
                        yg_control::RuleAction::Exclude
                    } else {
                        yg_control::RuleAction::Include
                    },
                    applies_to_private: false,
                })
            )
        };
        let (discovery, rule) = tokio::time::timeout(Duration::from_secs(10), pair)
            .await
            .unwrap_or_else(|_| panic!("round {round} timed out waiting on overlapping locks"));
        discovery.unwrap_or_else(|error| panic!("discovery round {round} failed: {error:#}"));
        rule.unwrap_or_else(|error| panic!("rule round {round} failed: {error:#}"));
    }
}

#[tokio::test]
async fn overlapping_sweeps_cannot_prune_a_later_observed_conflict() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "git",
            base_url: "https://sweep-race.example",
            token_env: None,
            api_root: None,
            slug: "acme/conflict",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();
    let connected = control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "git",
            base_url: "http://sweep-race.example",
            org_slug: "acme",
            token_env: None,
            api_root: Some("http://sweep-race.example/api"),
        })
        .await
        .unwrap();

    // Hold the first sweep inside its per-repo transaction after it acquires
    // the Forge lock. Without whole-sweep serialization, later sweeps queue
    // ahead of the first sweep's final prune and their newly recorded
    // conflict gets erased. Launch more waiters than the control-plane pool
    // can hold to also prove advisory waiters do not starve the lock owner.
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION pause_discovery_sweep() RETURNS trigger
         LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.slug = 'acme/pause' THEN
                 PERFORM pg_sleep(0.3);
             END IF;
             RETURN NEW;
         END
         $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER pause_discovery_sweep
         BEFORE INSERT OR UPDATE ON repos
         FOR EACH ROW EXECUTE FUNCTION pause_discovery_sweep()",
    )
    .execute(&pool)
    .await
    .unwrap();

    let first_control = control.clone();
    let org_id = connected.org_id;
    let first = tokio::spawn(async move {
        first_control
            .discover_forge_repos(
                org_id,
                &[yg_control::DiscoveredRepo {
                    slug: "acme/pause",
                    visibility: yg_control::RepoVisibility::Public,
                    fetch_depth: None,
                }],
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let later: Vec<_> = (0..8)
        .map(|_| {
            let later_control = control.clone();
            tokio::spawn(async move {
                later_control
                    .discover_forge_repos(
                        org_id,
                        &[yg_control::DiscoveredRepo {
                            slug: "acme/conflict",
                            visibility: yg_control::RepoVisibility::Public,
                            fetch_depth: None,
                        }],
                    )
                    .await
            })
        })
        .collect();
    first.await.unwrap().unwrap();
    for sweep in later {
        sweep.await.unwrap().unwrap();
    }

    let conflicts = control.admin_discovery_conflicts().await.unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].slug.as_str(), "acme/conflict");
}

#[test]
fn admin_status_dto_accepts_an_absent_additive_conflict_list() {
    let status: yg_verbs::admin::AdminStatusResponse = serde_json::from_value(serde_json::json!({
        "repos": [],
        "visibility_counts": {"public": 0, "internal": 0, "private": 0}
    }))
    .unwrap();
    assert!(status.discovery_conflicts.is_empty());
}
