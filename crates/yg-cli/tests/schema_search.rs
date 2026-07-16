//! Schema-v7 lexical-search behavior through a real spawned server.
//!
//! This target compiles in the normal workspace checks and runs against the
//! dev compose stack with the other spawned-process e2e targets.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

async fn search(base: &str, query: &str) -> serde_json::Value {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/verbs/search"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({"query": query, "kinds": ["File"], "limit": 20}))
        .send()
        .await
        .expect("search request");
    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("search response JSON");
    assert!(status.is_success(), "search failed with {status}: {body}");
    body
}

async fn await_repo_hits(base: &str, query: &str, repos: &[&str]) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let body = search(base, query).await;
        let hits = body["hits"].as_array().expect("search hits");
        if repos
            .iter()
            .all(|repo| hits.iter().any(|hit| hit["repo"].as_str() == Some(*repo)))
        {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "query {query:?} never found every repo {repos:?}: {body}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_v7_searches_code_paths_and_normalizes_cross_repo_rank() {
    let (feature_guard, feature_repo, feature_url) = fixture_repo_with(&[(
        "services/http/request_router.rs",
        "fn enforceRateLimit() { consume_budget(); }\n",
    )]);
    let (small_guard, small_repo, small_url) = fixture_repo_with(&[
        ("rank/strong.txt", "quality marker quality marker\n"),
        ("rank/good.txt", "quality marker\n"),
    ]);

    let mut large_files = vec![
        (
            "rank/strong.txt".to_string(),
            "quality marker quality marker\n".to_string(),
        ),
        ("rank/good.txt".to_string(), "quality marker\n".to_string()),
    ];
    for ordinal in 0..128 {
        large_files.push((
            format!("filler/document_{ordinal:03}.txt"),
            format!("unrelated padding {ordinal}\n"),
        ));
    }
    let large_refs = large_files
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect::<Vec<_>>();
    let (large_guard, large_repo, large_url) = fixture_repo_with(&large_refs);

    let db_name = create_test_db().await;
    let (_server, base) = spawn_yg_serve(&db_name, |command| {
        command
            .env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN)
            .env("YG_GIT_CACHE", feature_guard.path().join("git-cache"));
    });
    for url in [feature_url, small_url, large_url] {
        let response = post_repo(&base, json!({"url": url})).await;
        assert!(
            response.status().is_success(),
            "repo add failed: {response:?}"
        );
    }

    let small = small_repo.display().to_string();
    let large = large_repo.display().to_string();
    let feature = feature_repo.display().to_string();

    let body = await_repo_hits(&base, "quality marker", &[&small, &large]).await;
    let ranking_hits = body["hits"].as_array().expect("ranking hits");
    assert_eq!(
        ranking_hits.len(),
        4,
        "only the two relevance tiers in each repository match: {body}"
    );
    let scores = ranking_hits
        .iter()
        .map(|hit| hit["score"].as_f64().expect("hit score"))
        .collect::<Vec<_>>();
    assert!(
        scores[..2]
            .iter()
            .all(|strong| scores[2..].iter().all(|good| strong > good)),
        "both strong hits rank above both good hits: {body}"
    );
    assert!(
        scores[2..].iter().all(|score| *score > 0.0 && *score < 1.0),
        "second-tier scores are strictly normalized between zero and one: {body}"
    );
    // Corpus-specific BM25 statistics make exact normalized scores unstable even
    // when the relevance tier agrees, so cross-repository tier-2 scores may vary
    // by up to 5% while still demonstrating comparable federated ranking.
    let tier_two_max = scores[2].max(scores[3]);
    assert!(
        (scores[2] - scores[3]).abs() <= tier_two_max * 0.05,
        "the second relevance tier is comparable within 5% across corpus sizes: {body}"
    );
    // Within a tier, cross-repo order rides on sub-percent score noise
    // (and the random tmp-path repo keys), so assert tiers as sets: both
    // strong hits precede both good hits, each tier covering both repos.
    let tier = |slice: &[serde_json::Value]| {
        let mut pairs = slice
            .iter()
            .map(|hit| {
                (
                    hit["repo"].as_str().expect("hit repo").to_string(),
                    hit["path"].as_str().expect("hit path").to_string(),
                )
            })
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        pairs
    };
    let expected_tier = |path: &str| {
        let mut pairs = vec![
            (small.clone(), path.to_string()),
            (large.clone(), path.to_string()),
        ];
        pairs.sort_unstable();
        pairs
    };
    assert_eq!(
        tier(&ranking_hits[..2]),
        expected_tier("rank/strong.txt"),
        "the strong tier covers both repositories first"
    );
    assert_eq!(
        tier(&ranking_hits[2..4]),
        expected_tier("rank/good.txt"),
        "the good tier follows with both repositories"
    );

    let body = await_repo_hits(&base, "rate limit", &[&feature]).await;
    assert!(
        body["hits"]
            .as_array()
            .expect("body hits")
            .iter()
            .any(|hit| {
                hit["repo"].as_str() == Some(&feature)
                    && hit["path"].as_str() == Some("services/http/request_router.rs")
            }),
        "camel-case body identifiers split into queryable words: {body}"
    );

    for query in ["services", "router"] {
        let body = search(&base, query).await;
        assert!(
            body["hits"]
                .as_array()
                .expect("path hits")
                .iter()
                .any(|hit| {
                    hit["repo"].as_str() == Some(&feature)
                        && hit["path"].as_str() == Some("services/http/request_router.rs")
                }),
            "directory and filename fragments are searchable through the path field: {body}"
        );
    }

    drop((small_guard, large_guard));
}
