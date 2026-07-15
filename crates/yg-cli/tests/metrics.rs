//! Prometheus exposition across the real `serve --role=all` seam.

mod common;

use common::*;

async fn scrape(base: &str, token: Option<&str>) -> reqwest::Response {
    let request = reqwest::Client::new().get(format!("{base}/metrics"));
    match token {
        Some(token) => request.bearer_auth(token).send().await.unwrap(),
        None => request.send().await.unwrap(),
    }
}

fn sample_value(text: &str, name: &str, labels: &[&str]) -> Option<f64> {
    text.lines().find_map(|line| {
        (!line.starts_with('#')
            && line.starts_with(name)
            && labels.iter().all(|label| line.contains(label)))
        .then(|| {
            line.split_ascii_whitespace()
                .last()
                .and_then(|value| value.parse::<f64>().ok())
        })
        .flatten()
    })
}

fn sample_is_positive(text: &str, name: &str, labels: &[&str]) -> bool {
    sample_value(text, name, labels).is_some_and(|value| value > 0.0)
}

async fn wait_for_sample_above(base: &str, name: &str, labels: &[&str], previous: f64) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let response = scrape(base, Some(TEST_TOKEN)).await;
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        if sample_value(&text, name, labels).is_some_and(|value| value > previous) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{name} never advanced past {previous}; last scrape:\n{text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// multi_thread: the spawned server blocks a process thread while this
/// runtime drives HTTP requests and waits for its worker loops.
#[tokio::test(flavor = "multi_thread")]
async fn indexing_and_queries_move_prometheus_metrics() {
    let (fixture, _repo_dir, fixture_url) = go_fixture_repo();
    let db_name = create_test_db().await;
    let (_server, base) = spawn_yg_serve(&db_name, |cmd| {
        cmd.env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN)
            .env("YG_GIT_CACHE", fixture.path().join("git-cache"))
            .env("YG_POLL_INTERVAL", "1");
    });

    let unauthorized = scrape(&base, None).await;
    assert_eq!(unauthorized.status(), 401);
    let member: serde_json::Value = reqwest::Client::new()
        .post(format!("{base}/v1/admin/tokens"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({"member": "metrics-reader"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let member_forbidden = scrape(&base, member["token"].as_str()).await;
    assert_eq!(member_forbidden.status(), 403);

    let added = post_repo(
        &base,
        serde_json::json!({"url": fixture_url, "poll_interval": 1}),
    )
    .await;
    assert!(added.status().is_success());
    await_symbol(&base, "Hello", std::time::Duration::from_secs(30)).await;

    // A budget-deferred poll is precisely the lag observation most likely to
    // reveal a falling-behind Forge. Force one normal poll to spend the only
    // token, then force the same repo due again and require the histogram to
    // advance even though the rate check defers it without a network request.
    let poll_lag_count = "yggdrasil_forge_poll_lag_observations_seconds_count";
    let forge_label = ["forge=\""];
    let before = scrape(&base, Some(TEST_TOKEN)).await.text().await.unwrap();
    let before = sample_value(&before, poll_lag_count, &forge_label).unwrap_or(0.0);
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{db_name}"))
        .await
        .unwrap();
    sqlx::query("UPDATE forges SET rate_budget = 1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE repos SET next_poll_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    let first = wait_for_sample_above(&base, poll_lag_count, &forge_label, before).await;
    let after_first = sample_value(&first, poll_lag_count, &forge_label).unwrap();
    sqlx::query("UPDATE repos SET next_poll_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    let _deferred_poll =
        wait_for_sample_above(&base, poll_lag_count, &forge_label, after_first).await;
    let defer_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let retry_seconds = loop {
        let seconds: f64 = sqlx::query_scalar(
            "SELECT extract(epoch FROM next_poll_at - now())::float8 FROM repos LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if seconds >= 40.0 {
            break seconds;
        }
        assert!(
            std::time::Instant::now() < defer_deadline,
            "poll-lag count advanced but the rate defer did not settle; last retry was {seconds}s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(
        (40.0..=70.0).contains(&retry_seconds),
        "second poll must be rate-deferred on the one-per-minute budget, got {retry_seconds}s"
    );

    let client = reqwest::Client::new();
    let search = client
        .post(format!("{base}/v1/verbs/search"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({"query": "Hello"}))
        .send()
        .await
        .unwrap();
    assert!(search.status().is_success());
    let search: serde_json::Value = search.json().await.unwrap();
    let id = search["hits"][0]["id"].as_str().unwrap();
    for (verb, body) in [
        ("node", serde_json::json!({"id": id})),
        ("neighbors", serde_json::json!({"id": id})),
        ("history", serde_json::json!({"id": id})),
    ] {
        let response = client
            .post(format!("{base}/v1/verbs/{verb}"))
            .bearer_auth(TEST_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{verb} returned {response:?}"
        );
    }

    let response = scrape(&base, Some(TEST_TOKEN)).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();

    assert!(text.contains("yggdrasil_job_queue_depth"));
    for kind in ["fetch", "index"] {
        assert!(sample_is_positive(
            &text,
            "yggdrasil_job_claim_latency_seconds_count",
            &[&format!("kind=\"{kind}\"")],
        ));
        assert!(sample_is_positive(
            &text,
            "yggdrasil_job_outcomes_total",
            &[&format!("kind=\"{kind}\""), "outcome=\"success\""],
        ));
        assert!(sample_is_positive(
            &text,
            "yggdrasil_job_duration_seconds_count",
            &[&format!("kind=\"{kind}\"")],
        ));
    }
    for verb in ["node", "neighbors", "history", "search"] {
        assert!(sample_is_positive(
            &text,
            "yggdrasil_verb_request_duration_seconds_count",
            &[&format!("verb=\"{verb}\"")],
        ));
    }
    assert!(sample_is_positive(
        &text,
        "yggdrasil_shard_cache_misses_total",
        &["artifact=\"graph\""],
    ));
    assert!(sample_is_positive(
        &text,
        "yggdrasil_shard_cache_hits_total",
        &["artifact=\"graph\""],
    ));
    assert!(text.contains("yggdrasil_shard_cache_evictions_total"));
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_can_be_exposed_for_an_unauthenticated_scraper() {
    let db_name = create_test_db().await;
    let (_server, base) = spawn_yg_api(&db_name, |cmd| {
        cmd.env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN)
            .env("YG_METRICS_UNAUTHENTICATED", "true");
    });

    let response = scrape(&base, None).await;
    assert_eq!(response.status(), 200);
    assert!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/openmetrics-text")
    );
    assert!(response.text().await.unwrap().ends_with("# EOF\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_role_can_expose_its_process_local_metrics() {
    let db_name = create_test_db().await;
    let (_worker, announcement) = spawn_yg_role("worker", &db_name, |cmd| {
        cmd.env("YG_WORKER_METRICS_ADDR", "127.0.0.1:0")
            .env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN);
    });
    let base = announcement
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("unexpected worker announcement: {announcement}"));

    assert_eq!(scrape(base, None).await.status(), 401);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let body = loop {
        let response = scrape(base, Some(TEST_TOKEN)).await;
        assert_eq!(response.status(), 200);
        assert!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("application/openmetrics-text")
        );
        let body = response.text().await.unwrap();
        if ["fetch", "index"].into_iter().all(|kind| {
            body.lines().any(|line| {
                line.starts_with(&format!("yggdrasil_job_queue_depth{{kind=\"{kind}\"}} "))
            })
        }) {
            break body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker queue-depth gauges never appeared; last scrape:\n{body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(body.ends_with("# EOF\n"));
}
