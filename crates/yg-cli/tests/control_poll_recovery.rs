mod common;

use std::time::Duration;

use common::{DEV_POSTGRES, control_plane, create_test_db};

#[tokio::test]
async fn invalid_persisted_poll_heads_are_recovered_at_claim_and_record() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .add_repo(yg_control::AddRepo {
            forge_kind: "git",
            base_url: "https://git.example.test",
            token_env: None,
            api_root: None,
            slug: "acme/widgets",
            fetch_depth: None,
            poll_interval_seconds: None,
        })
        .await
        .unwrap();
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    let repo_id: i64 = sqlx::query_scalar("SELECT id FROM repos")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE repos
         SET last_synced_commit = '0123456789abcdef0123456789abcdef01234567'
         WHERE id = $1",
    )
    .bind(repo_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "UPDATE repos
         SET poll_observed_head = 'not-a-commit-sha', next_poll_at = now()
         WHERE id = $1",
    )
    .bind(repo_id)
    .execute(&pool)
    .await
    .unwrap();
    let claimed = control
        .claim_due_poll(Duration::from_secs(300), 0.0)
        .await
        .unwrap()
        .expect("a malformed observed head must not prevent claiming the due repo");
    assert!(
        claimed.observed_head().is_none(),
        "claiming must expose a malformed persisted head as absent"
    );
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT poll_observed_head FROM repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        None,
        "claiming must remove the poisoned value"
    );

    sqlx::query("UPDATE repos SET poll_observed_head = 'still-not-a-sha' WHERE id = $1")
        .bind(repo_id)
        .execute(&pool)
        .await
        .unwrap();
    let outcome = control
        .record_poll_observation(
            repo_id,
            &yg_control::PollValidators::default(),
            &yg_control::PollHeadObservation::NotModified,
        )
        .await
        .unwrap();
    assert_eq!(outcome, yg_control::PollRecordOutcome::Unchanged);
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT poll_observed_head FROM repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        None,
        "recording must remove a poisoned value instead of failing typed decoding"
    );
}
