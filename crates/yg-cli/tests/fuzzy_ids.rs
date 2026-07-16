//! Fuzzy node addressing across every node-addressed Verb. This target
//! compiles in the normal workspace checks and runs against the dev compose
//! stack with the other e2e targets.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn fuzzy_symbols_resolve_uniquely_or_return_ranked_candidates() {
    let h = Harness::boot_with(&[
        (
            "alpha/service.go",
            "package alpha\n\nfunc Resolve() {}\n\nfunc Unique() {}\n",
        ),
        ("beta/service.go", "package beta\n\nfunc Resolve() {}\n"),
    ])
    .await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo = h.qualifier();

    let exact_unique = format!("sym:{repo}:alpha/service.go#Unique");
    for verb in ["node", "neighbors", "history"] {
        let exact = h.verb_ok(verb, json!({"id": exact_unique})).await;
        let fuzzy = h.verb_ok(verb, json!({"id": "Unique", "repo": repo})).await;
        assert_eq!(
            fuzzy, exact,
            "unique fuzzy {verb} behaves like the exact id"
        );
    }

    let client = reqwest::Client::new();
    let endpoint = format!("{}/v1/verbs/node", h.base);
    let exact_bytes = client
        .post(&endpoint)
        .bearer_auth(TEST_TOKEN)
        .json(&json!({"id": exact_unique}))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let fuzzy_bytes = client
        .post(&endpoint)
        .bearer_auth(TEST_TOKEN)
        .json(&json!({"id": "Unique", "repo": repo}))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        fuzzy_bytes, exact_bytes,
        "resolved wire bytes are identical"
    );

    let (status, ambiguous) = h.verb("node", json!({"id": "Resolve", "repo": repo})).await;
    assert_eq!(status, 200, "ambiguity is a candidate result, not an error");
    assert_eq!(ambiguous["resolution"], "ambiguous");
    let candidates = ambiguous["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2, "both declarations are retained");
    let ids: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate["id"].as_str().expect("candidate id"))
        .collect();
    assert!(ids[0] < ids[1], "confidence ties break by canonical id");
    for candidate in candidates {
        assert_eq!(candidate["kind"], "Symbol");
        assert!(candidate["path"].is_string());
        let confidence = candidate["confidence"].as_f64().expect("confidence");
        assert!(
            (confidence - 0.45).abs() < 1e-9,
            "borrowed syntactic-pass spread convention: {candidate}"
        );
    }

    let exact_alpha = format!("sym:{repo}:alpha/service.go#Resolve");
    let narrowed = h
        .verb_ok(
            "node",
            json!({"id": "Resolve", "repo": repo, "path": "alpha/"}),
        )
        .await;
    let exact = h.verb_ok("node", json!({"id": exact_alpha})).await;
    assert_eq!(
        narrowed, exact,
        "a path fragment narrows to one declaration"
    );

    let (status, missing) = h.verb("node", json!({"id": "Missing", "repo": repo})).await;
    assert_eq!(status, 404);
    assert_eq!(missing["error"]["kind"], "no_such_symbol");
    assert!(missing["error"].get("candidates").is_none());

    let (status, wrong_case) = h.verb("node", json!({"id": "resolve", "repo": repo})).await;
    assert_eq!(status, 404);
    assert_eq!(wrong_case["error"]["kind"], "no_such_symbol");

    let human = h.yg_ok(&["node", "Resolve", "--repo", &repo]).await;
    assert!(human.contains("ambiguous symbol"), "{human}");
    assert!(
        human.contains("alpha/service.go") && human.contains("beta/service.go"),
        "{human}"
    );
    assert!(
        human.contains("0.450000"),
        "confidence is rendered: {human}"
    );
}

#[tokio::test]
async fn fuzzy_ambiguity_caps_rendered_candidates_and_reports_the_exact_count() {
    let files = (0..=yg_verbs::MAX_ADDRESS_CANDIDATES)
        .map(|index| {
            (
                format!("pkg{index:02}/service.go"),
                format!("package pkg{index:02}\n\nfunc Resolve() {{}}\n"),
            )
        })
        .collect::<Vec<_>>();
    let file_refs = files
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect::<Vec<_>>();
    let h = Harness::boot_with(&file_refs).await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo = h.qualifier();

    let ambiguous = h
        .verb_ok("node", json!({"id": "Resolve", "repo": repo}))
        .await;
    let candidates = ambiguous["candidates"].as_array().expect("candidates");
    let total_matches = yg_verbs::MAX_ADDRESS_CANDIDATES + 1;
    assert_eq!(ambiguous["total_matches"], total_matches);
    assert_eq!(candidates.len(), yg_verbs::MAX_ADDRESS_CANDIDATES);
    let expected_confidence = yg_shard::SYNTACTIC_MATCH / total_matches as f64;
    for pair in candidates.windows(2) {
        assert!(pair[0]["id"].as_str() < pair[1]["id"].as_str());
    }
    for candidate in candidates {
        let confidence = candidate["confidence"].as_f64().expect("confidence");
        assert!((confidence - expected_confidence).abs() < 1e-9);
    }

    let human = h.yg_ok(&["node", "Resolve", "--repo", &repo]).await;
    assert!(
        human.contains(&format!(
            "showing {} of {total_matches} matches",
            yg_verbs::MAX_ADDRESS_CANDIDATES
        )),
        "{human}"
    );
}
