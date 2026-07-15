//! Graceful process-shutdown coverage against the dev compose stack.

mod common;

use common::*;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn combined_sigterm_drains_an_in_flight_request_before_clean_exit() {
    let db_name = create_test_db().await;
    let _migrated = control_plane(&db_name).await;
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE repos IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();

    let (mut server, base) = spawn_yg_serve(&db_name, |command| {
        command.env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN);
    });
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("{base}/v1/admin/status"))
            .bearer_auth(TEST_TOKEN)
            .send()
            .await
    });
    await_handler_lock(&db_name).await;

    let signal = std::process::Command::new("kill")
        .args(["-TERM", &server.0.id().to_string()])
        .status()
        .expect("sending SIGTERM");
    assert!(signal.success(), "kill -TERM must succeed");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        server.0.try_wait().unwrap().is_none(),
        "the process must remain alive while its request is in flight"
    );
    assert!(
        !request.is_finished(),
        "the database lock must still hold the request open"
    );

    lock.rollback().await.unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .expect("the drained request must finish")
        .expect("request task must not panic")
        .expect("request must complete successfully");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let status = await_process_exit(&mut server).await;
    assert!(
        status.success(),
        "SIGTERM must produce a clean exit: {status}"
    );
}

#[tokio::test]
async fn released_index_lease_is_immediately_reclaimable_and_stays_fenced() {
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
    let fetch = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("fetch job must be claimable");
    control
        .complete_fetch(&fetch, "feedface0000000000000000000000000000feed")
        .await
        .unwrap();

    let released = control
        .claim_due_index(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("index job must be claimable");
    assert!(control.release_index(&released).await.unwrap());

    let retry = control
        .claim_due_index(std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .expect("shutdown release must make the job immediately due");
    assert_eq!(retry.job_id, released.job_id, "the same job must retry");
    assert_eq!(retry.attempts, 0, "shutdown is not a failed attempt");
    assert!(
        !control.release_index(&released).await.unwrap(),
        "the old claim must not release the healthy retry"
    );
    assert!(control.release_index(&retry).await.unwrap());
}

#[cfg(unix)]
async fn await_handler_lock(db_name: &str) {
    let admin = admin_pool().await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (blocked,): (bool,) = sqlx::query_as(
                "SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                    WHERE datname = $1
                      AND wait_event_type = 'Lock'
                      AND query LIKE '%SELECT r.slug, f.base_url AS forge%'
                )",
            )
            .bind(db_name)
            .fetch_one(&admin)
            .await
            .unwrap();
            if blocked {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("status handler must block on the repos table lock");
}

#[cfg(unix)]
async fn await_process_exit(server: &mut KillOnDrop) -> std::process::ExitStatus {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("server must exit after its request drains")
}
